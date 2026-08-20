//! Native Pi harness over `pi --mode rpc`.
//!
//! Pi's wire is JSONL, not JSON-RPC 2.0: commands and responses carry a
//! string `id`, while asynchronous agent and extension-UI events share the
//! same stdout stream. The reader therefore owns stdout and multiplexes
//! responses by id; no command waiter is allowed to consume and discard an
//! unrelated event.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, ToolCall, UserInputAnswer, UserInputQuestion,
};

use crate::{
    Harness, HarnessError, RunControls, StderrTail, SteerMessage, crash_message, shutdown_child,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOOL_OUTPUT_LIMIT: usize = 64 * 1024;

type RequestInput =
    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PI_EXECUTABLE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let executable = if cfg!(windows) { "pi.cmd" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/bin").join(executable));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(executable));
    candidates.push(PathBuf::from("/usr/local/bin").join(executable));
    candidates.into_iter().find(|path| path.is_file())
}

#[derive(Debug)]
enum Incoming {
    Event(Value),
    ProtocolError(String),
    Eof,
}

#[derive(Clone)]
struct PiRpcClient {
    next_id: Arc<AtomicU64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<Value>,
}

impl PiRpcClient {
    fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer, writer_rx) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicU64::new(0)),
                pending,
                writer,
            },
            incoming_rx,
        )
    }

    async fn request(&self, mut command: Value) -> Result<Value, HarnessError> {
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_owned();
        let id = format!("zeron-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        command["id"] = Value::String(id.clone());
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("Pi pending lock")
            .insert(id.clone(), tx);
        if self.writer.send(command).is_err() {
            self.pending.lock().expect("Pi pending lock").remove(&id);
            return Err(HarnessError::Protocol(format!("{kind}: Pi stdin closed")));
        }
        let outcome = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        let response = match outcome {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(error))) => return Err(HarnessError::Protocol(format!("{kind}: {error}"))),
            Ok(Err(_)) => {
                return Err(HarnessError::Protocol(format!(
                    "{kind}: Pi exited before responding"
                )));
            }
            Err(_) => {
                self.pending.lock().expect("Pi pending lock").remove(&id);
                return Err(HarnessError::Protocol(format!(
                    "{kind}: timed out waiting for Pi response"
                )));
            }
        };
        if !response
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("command failed");
            return Err(HarnessError::Protocol(format!("{kind}: {error}")));
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }

    fn send(&self, command: Value) -> Result<(), HarnessError> {
        self.writer
            .send(command)
            .map_err(|_| HarnessError::Protocol("Pi stdin closed".into()))
    }
}

async fn write_loop(mut stdin: ChildStdin, mut commands: mpsc::UnboundedReceiver<Value>) {
    while let Some(command) = commands.recv().await {
        let mut line = match serde_json::to_vec(&command) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(target: "zeron_harness::pi", %error, "failed to encode Pi command");
                continue;
            }
        };
        line.push(b'\n');
        if let Err(error) = stdin.write_all(&line).await {
            tracing::debug!(target: "zeron_harness::pi", %error, "Pi stdin closed");
            return;
        }
        if let Err(error) = stdin.flush().await {
            tracing::debug!(target: "zeron_harness::pi", %error, "Pi stdin flush failed");
            return;
        }
    }
}

async fn read_loop(stdout: ChildStdout, pending: Pending, incoming: mpsc::Sender<Incoming>) {
    // Tokio's `lines()` splits only on LF and strips an optional CR, matching
    // Pi's strict JSONL framing (unlike Node's readline Unicode separators).
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.is_empty() {
                    continue;
                }
                let message: Value = match serde_json::from_str(&line) {
                    Ok(message) => message,
                    Err(error) => {
                        let detail = format!("invalid JSONL from Pi: {error}");
                        if incoming
                            .send(Incoming::ProtocolError(detail))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                if message.get("type").and_then(Value::as_str) == Some("response") {
                    let Some(id) = message.get("id").and_then(Value::as_str) else {
                        let _ = incoming
                            .send(Incoming::ProtocolError(
                                "Pi response omitted request id".into(),
                            ))
                            .await;
                        continue;
                    };
                    if let Some(waiter) = pending.lock().expect("Pi pending lock").remove(id) {
                        let _ = waiter.send(Ok(message));
                    }
                    continue;
                }
                if incoming.send(Incoming::Event(message)).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                let _ = incoming
                    .send(Incoming::ProtocolError(format!(
                        "reading Pi stdout: {error}"
                    )))
                    .await;
                break;
            }
        }
    }
    pending.lock().expect("Pi pending lock").clear();
    let _ = incoming.send(Incoming::Eof).await;
}

fn pi_tool_call(tool_name: &str, args: &Value) -> ToolCall {
    let string = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| args.get(key).and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    match tool_name {
        "read" => ToolCall::ReadFile {
            path: string(&["path", "file_path"]).unwrap_or_default(),
        },
        "write" => ToolCall::WriteFile {
            path: string(&["path", "file_path"]).unwrap_or_default(),
            content: None,
        },
        "edit" => ToolCall::EditFile {
            path: string(&["path", "file_path"]).unwrap_or_default(),
            old_string: string(&["oldText", "old_string"]),
            new_string: string(&["newText", "new_string"]),
        },
        "bash" => ToolCall::Exec {
            command: string(&["command", "cmd"]).unwrap_or_default(),
        },
        "grep" | "search" => ToolCall::Search {
            pattern: string(&["pattern", "query"]).unwrap_or_default(),
            path: string(&["path", "dir"]),
        },
        "find" | "glob" => ToolCall::Glob {
            pattern: string(&["pattern"]).unwrap_or_default(),
        },
        "web_search" => ToolCall::WebSearch {
            query: string(&["query"]).unwrap_or_default(),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: string(&["url"]).unwrap_or_default(),
            prompt: string(&["prompt"]),
        },
        name if name.starts_with("mcp__") => {
            let rest = &name[5..];
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            ToolCall::Mcp {
                server: server.into(),
                tool: tool.into(),
                input: Some(args.clone()),
            }
        }
        name => ToolCall::Unknown {
            name: name.into(),
            input: Some(args.clone()),
        },
    }
}

fn text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(text) = value.get("content").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let content = value.get("content").and_then(Value::as_array)?;
    let mut output = String::new();
    for block in content {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(text);
        }
    }
    (!output.is_empty()).then_some(output)
}

fn capped_output(value: &Value) -> Option<String> {
    let mut output = text_content(value).or_else(|| {
        value
            .get("stdout")
            .or_else(|| value.get("output"))
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    })?;
    if output.len() > TOOL_OUTPUT_LIMIT {
        output.truncate(TOOL_OUTPUT_LIMIT);
        output.push_str("\n… output truncated by Zeron");
    }
    Some(output)
}

fn usage_event(message: &Value) -> Option<AgentEvent> {
    let usage = message.get("usage")?;
    Some(AgentEvent::Usage {
        input_tokens: usage.get("input").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: usage.get("output").and_then(Value::as_u64).unwrap_or(0),
    })
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

struct Normalizer {
    assistant_message_id: String,
    assistant_message_open: bool,
    saw_text_delta: bool,
    emitted_tools: HashSet<String>,
    terminal_error: Option<String>,
}

impl Normalizer {
    fn new() -> Self {
        Self {
            assistant_message_id: new_message_id(),
            assistant_message_open: false,
            saw_text_delta: false,
            emitted_tools: HashSet::new(),
            terminal_error: None,
        }
    }

    fn rotate(&mut self) -> (String, String) {
        let next = new_message_id();
        let previous = std::mem::replace(&mut self.assistant_message_id, next.clone());
        (previous, next)
    }

    fn normalize(&mut self, event: &Value) -> Vec<AgentEvent> {
        let mut output = Vec::new();
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                self.assistant_message_open =
                    event["message"]["role"].as_str() == Some("assistant");
                self.saw_text_delta = false;
            }
            "message_update" => {
                let update = &event["assistantMessageEvent"];
                match update.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        if let Some(delta) = update.get("delta").and_then(Value::as_str) {
                            self.saw_text_delta = true;
                            output.push(AgentEvent::TextDelta { text: delta.into() });
                        }
                    }
                    "thinking_delta" => {
                        if let Some(delta) = update.get("delta").and_then(Value::as_str) {
                            output.push(AgentEvent::ReasoningDelta { text: delta.into() });
                        }
                    }
                    "toolcall_end" => {
                        let tool = &update["toolCall"];
                        let id = tool.get("id").and_then(Value::as_str).unwrap_or("");
                        if !id.is_empty() && self.emitted_tools.insert(id.into()) {
                            let name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let args = tool.get("arguments").cloned().unwrap_or(Value::Null);
                            output.push(AgentEvent::ToolCall {
                                id: id.into(),
                                call: pi_tool_call(name, &args),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "tool_execution_start" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !id.is_empty() && self.emitted_tools.insert(id.into()) {
                    let name = event
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    output.push(AgentEvent::ToolCall {
                        id: id.into(),
                        call: pi_tool_call(name, &event["args"]),
                    });
                }
            }
            "tool_execution_end" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !id.is_empty() {
                    output.push(AgentEvent::ToolResult {
                        id: id.into(),
                        is_error: event
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        output: capped_output(&event["result"]),
                        diff: None,
                    });
                }
            }
            "message_end" => {
                let message = &event["message"];
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    if !self.saw_text_delta
                        && let Some(text) = text_content(message)
                    {
                        output.push(AgentEvent::TextDelta { text });
                    }
                    if let Some(usage) = usage_event(message) {
                        output.push(usage);
                    }
                    if message.get("stopReason").and_then(Value::as_str) == Some("error") {
                        let error = message
                            .get("errorMessage")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| text_content(message))
                            .unwrap_or_else(|| "Pi assistant turn failed".into());
                        self.terminal_error = Some(error.clone());
                        output.push(AgentEvent::Error { message: error });
                    }
                    let (completed, _) = self.rotate();
                    output.push(AgentEvent::AssistantMessageCompleted {
                        assistant_message_id: completed,
                    });
                }
                self.assistant_message_open = false;
                self.saw_text_delta = false;
            }
            "auto_retry_start" => output.push(AgentEvent::TextDelta {
                text: "\n_Retrying…_\n".into(),
            }),
            "compaction_start" => output.push(AgentEvent::TextDelta {
                text: "\n_Compacting context…_\n".into(),
            }),
            "auto_retry_end" if event.get("success").and_then(Value::as_bool) == Some(false) => {
                let error = event
                    .get("finalError")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi exhausted automatic retries")
                    .to_owned();
                self.terminal_error = Some(error.clone());
                output.push(AgentEvent::Error { message: error });
            }
            "extension_error" => {
                let error = event
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi extension failed");
                output.push(AgentEvent::Error {
                    message: error.into(),
                });
            }
            _ => {}
        }
        output
    }
}

fn to_pi_thinking(level: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match level? {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => "max",
    })
}

fn pi_model_parts(model: &str) -> Option<(&str, &str)> {
    let (provider, model_id) = model.split_once('/')?;
    (!provider.is_empty() && !model_id.is_empty()).then_some((provider, model_id))
}

fn image_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
}

async fn prompt_images(paths: &[String]) -> Result<Vec<Value>, HarnessError> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = tokio::fs::read(path).await.map_err(|error| {
            HarnessError::Protocol(format!("read Pi image attachment {path}: {error}"))
        })?;
        images.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": image_mime(Path::new(path)),
        }));
    }
    Ok(images)
}

fn available_model(value: &Value) -> Option<Model> {
    let provider = value.get("provider")?.as_str()?;
    let id = value.get("id")?.as_str()?;
    let label = value.get("name").and_then(Value::as_str).unwrap_or(id);
    let reasoning = value
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let multimodal = value
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")));
    let mut reasoning_levels = Vec::new();
    if reasoning {
        reasoning_levels.extend([
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
        ]);
        let map = value.get("thinkingLevelMap").and_then(Value::as_object);
        if map.is_some_and(|map| map.contains_key("xhigh")) {
            reasoning_levels.push(ReasoningLevel::XHigh);
        }
        if map.is_some_and(|map| map.contains_key("max")) {
            reasoning_levels.push(ReasoningLevel::Max);
        }
    }
    let capabilities = match (reasoning, multimodal) {
        (true, true) => "reasoning, multimodal",
        (true, false) => "reasoning",
        (false, true) => "non-reasoning, multimodal",
        (false, false) => "non-reasoning",
    };
    Some(Model {
        id: format!("{provider}/{id}"),
        label: label.into(),
        description: Some(format!("{provider} {capabilities} model via Pi")),
        reasoning_levels,
        options: Vec::new(),
    })
}

fn available_commands(data: &Value) -> Vec<SlashCommand> {
    data.get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            Some(SlashCommand {
                name: command.get("name")?.as_str()?.into(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                input_hint: None,
            })
        })
        .collect()
}

fn extension_question(event: &Value) -> Option<UserInputQuestion> {
    let method = event.get("method")?.as_str()?;
    if !matches!(method, "select" | "confirm" | "input" | "editor") {
        return None;
    }
    let id = event.get("id")?.as_str()?.to_owned();
    let title = event.get("title").and_then(Value::as_str).unwrap_or("Pi");
    let question = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(title);
    let options = if method == "confirm" {
        vec!["Yes".into(), "No".into()]
    } else {
        event
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    Some(UserInputQuestion {
        id,
        header: title.into(),
        question: question.into(),
        options,
        multi_select: false,
    })
}

fn answer_extension_ui(client: PiRpcClient, request_input: Arc<Box<RequestInput>>, event: Value) {
    let Some(question) = extension_question(&event) else {
        return;
    };
    let method = event
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let id = question.id.clone();
    tokio::spawn(async move {
        let answers = request_input(vec![question]).await.unwrap_or_default();
        let selected = answers
            .iter()
            .find(|answer| answer.question_id == id)
            .and_then(|answer| answer.labels.first())
            .cloned();
        let response = match (method.as_str(), selected) {
            ("confirm", Some(value)) => json!({
                "type": "extension_ui_response",
                "id": id,
                "confirmed": value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("allow"),
            }),
            (_, Some(value)) => json!({"type": "extension_ui_response", "id": id, "value": value}),
            _ => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        };
        let _ = client.send(response);
    });
}

fn cancel_extension_ui(client: &PiRpcClient, event: &Value) {
    if extension_question(event).is_some()
        && let Some(id) = event.get("id").and_then(Value::as_str)
    {
        let _ = client.send(json!({
            "type": "extension_ui_response",
            "id": id,
            "cancelled": true,
        }));
    }
}

pub struct PiNativeHarness {
    executable: Option<PathBuf>,
    kill_grace: Duration,
}

impl PiNativeHarness {
    pub fn new() -> Self {
        Self {
            executable: None,
            kill_grace: Duration::from_secs(3),
        }
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    fn executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable
            .clone()
            .or_else(resolve_pi_executable)
            .ok_or_else(|| {
                HarnessError::NotInstalled(
                    "pi CLI not found; install @earendil-works/pi-coding-agent".into(),
                )
            })
    }

    async fn spawn(
        &self,
        cwd: Option<&str>,
        no_session: bool,
    ) -> Result<(Child, PiRpcClient, mpsc::Receiver<Incoming>, StderrTail), HarnessError> {
        let executable = self.executable()?;
        let mut command = Command::new(&executable);
        command.args(["--mode", "rpc"]);
        if no_session {
            command.arg("--no-session");
        }
        crate::compose_child_path(&mut command, &executable);
        if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(executable.display().to_string())
            } else {
                HarnessError::Io(error)
            }
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("Pi child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("Pi child has no stdout".into()))?;
        let stderr_tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::pi", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }
        let (client, incoming) = PiRpcClient::new(stdin, stdout);
        Ok((child, client, incoming, stderr_tail))
    }

    async fn probe(&self, command: Value) -> Result<Value, HarnessError> {
        let (mut child, client, mut incoming, _stderr) = self.spawn(None, true).await?;
        let mut request = Box::pin(client.request(command));
        let result = loop {
            tokio::select! {
                response = &mut request => break response,
                message = incoming.recv() => match message {
                    Some(Incoming::Event(event)) => cancel_extension_ui(&client, &event),
                    Some(Incoming::ProtocolError(error)) => break Err(HarnessError::Protocol(error)),
                    Some(Incoming::Eof) | None => break Err(HarnessError::Protocol("Pi exited during discovery".into())),
                }
            }
        };
        shutdown_child(&mut child, self.kill_grace).await;
        result
    }
}

impl Default for PiNativeHarness {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Harness for PiNativeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn display_name(&self) -> &str {
        "Pi"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }

    fn installed(&self) -> bool {
        self.executable.is_some() || resolve_pi_executable().is_some()
    }

    fn deterministic_turn_end(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let data = self.probe(json!({"type": "get_available_models"})).await?;
        let models = data
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(available_model)
            .collect::<Vec<_>>();
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "get_available_models returned no authenticated Pi models".into(),
            ));
        }
        Ok(models)
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        let data = self.probe(json!({"type": "get_commands"})).await?;
        Ok(available_commands(&data))
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (child, client, incoming, stderr_tail) = self.spawn(Some(&request.cwd), false).await?;
        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(run_session(PiSession {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));
        Ok(
            futures::stream::unfold(event_rx, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })
            .boxed(),
        )
    }
}

struct PiSession {
    child: Child,
    client: PiRpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    kill_grace: Duration,
    stderr_tail: StderrTail,
}

async fn send_event(
    tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    event: AgentEvent,
) -> bool {
    tx.send(Ok(event)).await.is_ok()
}

async fn request_during_setup(
    client: &PiRpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    request_input: &Arc<Box<RequestInput>>,
    queued_events: &mut Vec<Value>,
    command: Value,
) -> Result<Value, HarnessError> {
    let mut request = Box::pin(client.request(command));
    loop {
        tokio::select! {
            response = &mut request => return response,
            message = incoming.recv() => match message {
                Some(Incoming::Event(event)) => {
                    if extension_question(&event).is_some() {
                        answer_extension_ui(client.clone(), Arc::clone(request_input), event);
                    } else {
                        queued_events.push(event);
                    }
                }
                Some(Incoming::ProtocolError(error)) => return Err(HarnessError::Protocol(error)),
                Some(Incoming::Eof) | None => return Err(HarnessError::Protocol("Pi exited during setup".into())),
            }
        }
    }
}

async fn finish_error(
    child: &mut Child,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    kill_grace: Duration,
    session_id: Option<String>,
    error: impl ToString,
) {
    let _ = send_event(
        event_tx,
        AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some(error.to_string()),
            session_id,
        },
    )
    .await;
    shutdown_child(child, kill_grace).await;
}

async fn run_session(session: PiSession) {
    let PiSession {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input: Arc<Box<RequestInput>> = Arc::new(request_input);
    let mut queued_events = Vec::new();

    let setup = async {
        if let Some(resume) = request
            .resume
            .as_deref()
            .filter(|resume| !resume.is_empty())
        {
            request_during_setup(
                &client,
                &mut incoming,
                &request_input,
                &mut queued_events,
                json!({"type": "switch_session", "sessionPath": resume}),
            )
            .await?;
        }
        request_during_setup(
            &client,
            &mut incoming,
            &request_input,
            &mut queued_events,
            json!({"type": "get_state"}),
        )
        .await?;
        if let Some((provider, model_id)) = request
            .model
            .as_deref()
            .filter(|model| *model != "default")
            .and_then(pi_model_parts)
        {
            request_during_setup(
                &client,
                &mut incoming,
                &request_input,
                &mut queued_events,
                json!({"type": "set_model", "provider": provider, "modelId": model_id}),
            )
            .await?;
        }
        if let Some(level) = to_pi_thinking(request.reasoning) {
            request_during_setup(
                &client,
                &mut incoming,
                &request_input,
                &mut queued_events,
                json!({"type": "set_thinking_level", "level": level}),
            )
            .await?;
        }
        // Refresh state after session/model changes so the resumable session
        // file returned to the engine is authoritative.
        let state = request_during_setup(
            &client,
            &mut incoming,
            &request_input,
            &mut queued_events,
            json!({"type": "get_state"}),
        )
        .await?;
        let resolved_session = state
            .get("sessionFile")
            .and_then(Value::as_str)
            .or_else(|| state.get("sessionId").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let images = prompt_images(&request.attachments).await?;
        let mut prompt = json!({"type": "prompt", "message": request.prompt});
        if !images.is_empty() {
            prompt["images"] = Value::Array(images);
        }
        request_during_setup(
            &client,
            &mut incoming,
            &request_input,
            &mut queued_events,
            prompt,
        )
        .await?;
        Ok::<String, HarnessError>(resolved_session)
    };

    let resolved_session = tokio::select! {
        result = setup => match result {
            Ok(session) => session,
            Err(error) => {
                finish_error(&mut child, &event_tx, kill_grace, None, error).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = send_event(&event_tx, AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            }).await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };
    let session_id = Some(resolved_session.clone());

    let mut normalizer = Normalizer::new();
    if !send_event(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: resolved_session,
            assistant_message_id: normalizer.assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let mut interrupted = false;
    let mut steering_open = true;
    let mut pending_events = std::collections::VecDeque::from(queued_events);
    loop {
        let message = if let Some(event) = pending_events.pop_front() {
            Some(Incoming::Event(event))
        } else {
            tokio::select! {
                message = incoming.recv() => message,
                steer = steering.recv(), if steering_open && !interrupted => {
                    match steer {
                        Some(SteerMessage { prompt, .. }) => {
                            match client.request(json!({"type": "steer", "message": prompt})).await {
                                Ok(_) => {
                                    let (previous, next) = normalizer.rotate();
                                    if !send_event(&event_tx, AgentEvent::Steered {
                                        assistant_message_id: Some(previous),
                                        next_assistant_message_id: Some(next),
                                    }).await {
                                        break;
                                    }
                                }
                                Err(error) => {
                                    let _ = send_event(&event_tx, AgentEvent::Error { message: error.to_string() }).await;
                                }
                            }
                        }
                        None => steering_open = false,
                    }
                    continue;
                }
                _ = interrupt.cancelled(), if !interrupted => {
                    interrupted = true;
                    if let Err(error) = client.request(json!({"type": "abort"})).await {
                        tracing::debug!(target: "zeron_harness::pi", %error, "Pi abort command failed");
                    }
                    continue;
                }
            }
        };

        match message {
            Some(Incoming::Event(event)) => {
                if event.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
                    answer_extension_ui(client.clone(), Arc::clone(&request_input), event);
                    continue;
                }
                if event.get("type").and_then(Value::as_str) == Some("agent_settled") {
                    let status = if interrupted {
                        DoneStatus::Interrupted
                    } else if normalizer.terminal_error.is_some() {
                        DoneStatus::Errored
                    } else {
                        DoneStatus::Completed
                    };
                    let error = normalizer.terminal_error.take();
                    let _ = send_event(
                        &event_tx,
                        AgentEvent::Done {
                            status,
                            result: None,
                            error,
                            session_id: session_id.clone(),
                        },
                    )
                    .await;
                    shutdown_child(&mut child, kill_grace).await;
                    return;
                }
                for event in normalizer.normalize(&event) {
                    if !send_event(&event_tx, event).await {
                        shutdown_child(&mut child, kill_grace).await;
                        return;
                    }
                }
            }
            Some(Incoming::ProtocolError(error)) => {
                finish_error(&mut child, &event_tx, kill_grace, session_id.clone(), error).await;
                return;
            }
            Some(Incoming::Eof) | None => {
                if interrupted {
                    let _ = send_event(
                        &event_tx,
                        AgentEvent::Done {
                            status: DoneStatus::Interrupted,
                            result: None,
                            error: None,
                            session_id: session_id.clone(),
                        },
                    )
                    .await;
                } else {
                    let status = child.try_wait().ok().flatten();
                    let error = crash_message("Pi", status, &stderr_tail);
                    finish_error(&mut child, &event_tx, kill_grace, session_id.clone(), error)
                        .await;
                    return;
                }
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        }
    }
    shutdown_child(&mut child, kill_grace).await;
}
