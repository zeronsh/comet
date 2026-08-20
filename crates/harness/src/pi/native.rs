//! Native Pi harness: spawns `pi --mode rpc` and speaks its JSONL protocol
//! directly. pi's RPC is request-response: each command gets an
//! `{"id":"...","type":"response","command":"...","success":true}` reply
//! before the next command can be sent.
//!
//! After the `prompt` response, async events stream on stdout:
//! `message_update` (text_delta, thinking_delta, tool calls),
//! `tool_execution_*`, `agent_start`, `agent_settled`, etc.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SlashCommand,
    SteeringMode, ToolCall, UserInputAnswer, UserInputQuestion,
};

use crate::pi::catalog::load_pi_models;
use crate::{Harness, HarnessError, RunControls, Signal, send_signal, shutdown_child};

/// Locate the pi binary.
fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PI_EXECUTABLE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "pi.cmd" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local").join("bin").join(exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(exe));
    candidates.push(PathBuf::from("/usr/local/bin").join(exe));
    candidates.into_iter().find(|p| p.is_file())
}

/// Map a pi tool name to a typed [`ToolCall`].
fn pi_tool_call(tool_name: &str, args: &Value) -> ToolCall {
    let raw_str = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match tool_name {
        "read" => ToolCall::ReadFile {
            path: raw_str("file_path")
                .or_else(|| raw_str("path"))
                .unwrap_or_default(),
        },
        "write" => ToolCall::WriteFile {
            path: raw_str("file_path")
                .or_else(|| raw_str("path"))
                .unwrap_or_default(),
            content: None,
        },
        "edit" => ToolCall::EditFile {
            path: raw_str("file_path")
                .or_else(|| raw_str("path"))
                .unwrap_or_default(),
            old_string: raw_str("oldText"),
            new_string: raw_str("newText"),
        },
        "bash" => ToolCall::Exec {
            command: raw_str("command")
                .or_else(|| raw_str("cmd"))
                .unwrap_or_default(),
        },
        "grep" | "search" => ToolCall::Search {
            pattern: raw_str("pattern")
                .or_else(|| raw_str("query"))
                .unwrap_or_default(),
            path: raw_str("path").or_else(|| raw_str("dir")),
        },
        "glob" => ToolCall::Glob {
            pattern: raw_str("pattern").unwrap_or_default(),
        },
        "web_search" => ToolCall::WebSearch {
            query: raw_str("query").unwrap_or_default(),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: raw_str("url").unwrap_or_default(),
            prompt: raw_str("prompt"),
        },
        "task" | "agent" => ToolCall::Unknown {
            name: raw_str("description")
                .unwrap_or_else(|| tool_name.to_string()),
            input: Some(args.clone()),
        },
        t if t.starts_with("mcp__") => {
            let rest = &t[5..];
            if let Some((server, tool)) = rest.split_once("__") {
                ToolCall::Mcp {
                    server: server.to_owned(),
                    tool: tool.to_owned(),
                    input: Some(args.clone()),
                }
            } else {
                ToolCall::Mcp {
                    server: rest.to_owned(),
                    tool: String::new(),
                    input: Some(args.clone()),
                }
            }
        }
        _ => ToolCall::Unknown {
            name: tool_name.to_string(),
            input: Some(args.clone()),
        },
    }
}

/// Parse a pi RPC event (a JSON value from a stdout line) into zero or more
/// [`AgentEvent`]s.
fn translate_pi_event(event: &Value) -> Vec<AgentEvent> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");

    match event_type {
        "message_update" => {
            let ame = match event.get("assistantMessageEvent") {
                Some(v) => v,
                None => return Vec::new(),
            };
            let ame_type = ame.get("type").and_then(Value::as_str).unwrap_or("");
            match ame_type {
                "text_delta" => {
                    if let Some(delta) = ame.get("delta").and_then(Value::as_str) {
                        vec![AgentEvent::TextDelta {
                            text: delta.to_string(),
                        }]
                    } else {
                        Vec::new()
                    }
                }
                "thinking_delta" => {
                    if let Some(delta) = ame.get("delta").and_then(Value::as_str) {
                        vec![AgentEvent::ReasoningDelta {
                            text: delta.to_string(),
                        }]
                    } else {
                        Vec::new()
                    }
                }
                "toolcall_start" | "toolcall_end" => {
                    let tc = ame
                        .get("toolCall")
                        .or_else(|| {
                            ame.get("partial")
                                .and_then(|p| p.get("content"))
                                .and_then(|c| c.get(0))
                        });
                    let tool_call_id = tc
                        .and_then(|t| t.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if tool_call_id.is_empty() || ame_type == "toolcall_delta" {
                        return Vec::new();
                    }
                    let tool_name = tc
                        .and_then(|t| t.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let args = tc
                        .and_then(|t| t.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let call = pi_tool_call(tool_name, &args);
                    vec![AgentEvent::ToolCall {
                        id: tool_call_id,
                        call,
                    }]
                }
                _ => Vec::new(),
            }
        }
        "tool_execution_end" => {
            let tool_call_id = event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if tool_call_id.is_empty() {
                return Vec::new();
            }
            let is_error = event.get("isError").and_then(Value::as_bool).unwrap_or(false);
            let result = event.get("result");
            let output = result.and_then(|r| {
                if let Some(text) = r.get("content").and_then(Value::as_str) {
                    Some(text.to_string())
                } else if let Some(arr) = r.get("content").and_then(Value::as_array) {
                    let texts: Vec<String> = arr
                        .iter()
                        .filter_map(|c| {
                            c.get("text").and_then(Value::as_str).map(str::to_string)
                        })
                        .collect();
                    if texts.is_empty() { None } else { Some(texts.join("\n")) }
                } else if let Some(text) = r.as_str() {
                    Some(text.to_string())
                } else {
                    r.get("stdout")
                        .or_else(|| r.get("output"))
                        .or_else(|| r.get("text"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            });
            vec![AgentEvent::ToolResult {
                id: tool_call_id,
                is_error,
                output,
                diff: None,
            }]
        }
        "agent_settled" | "agent_end" => {
            vec![AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            }]
        }
        "auto_retry_start" | "auto_compaction_start" => {
            let msg = match event_type {
                "auto_retry_start" => "Retrying...",
                _ => "Compacting context...",
            };
            vec![AgentEvent::TextDelta {
                text: format!("\n_{msg}_\n"),
            }]
        }
        _ => Vec::new(),
    }
}

/// Map Zeron's reasoning level to pi's thinking level.
fn to_pi_thinking(level: Option<ReasoningLevel>) -> Option<&'static str> {
    match level? {
        ReasoningLevel::Minimal => Some("minimal"),
        ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::XHigh => Some("xhigh"),
        ReasoningLevel::Max => Some("max"),
        ReasoningLevel::Ultra | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => {
            Some("max")
        }
    }
}

/// Parse pi's canonical model id (`provider/modelId`).
fn pi_model_parts(model_id: &str) -> (&str, &str) {
    match model_id.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("", model_id),
    }
}

/// Read and discard lines from stdout until we see a response with the
/// given id. Returns the response JSON, or errors on timeout.
async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: &str,
) -> Result<Value, HarnessError> {
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line_result = lines.next_line() => {
                let line = line_result?.unwrap_or_default();
                if line.trim().is_empty() {
                    continue;
                }
                let event: Value = serde_json::from_str(&line)
                    .map_err(|e| HarnessError::Protocol(format!("parse error: {e}")))?;
                let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                if event_type == "response"
                    && event.get("id").and_then(Value::as_str) == Some(expected_id)
                {
                    let success = event.get("success").and_then(Value::as_bool).unwrap_or(false);
                    if !success {
                        let err = event.get("error").and_then(Value::as_str).unwrap_or("unknown");
                        tracing::warn!(target: "zeron_harness::pi::native", %expected_id, %err, "pi command failed");
                    }
                    return Ok(event);
                }
                tracing::debug!(target: "zeron_harness::pi::native", %event_type, id = %expected_id, "skipping event while waiting for response");
            }
            _ = &mut deadline => {
                return Err(HarnessError::Protocol(format!(
                    "timeout waiting for response {expected_id}"
                )));
            }
        }
    }
}

/// The native Pi harness.
pub struct PiNativeHarness {
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

impl PiNativeHarness {
    pub fn new() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    fn pi_command(&self) -> Result<std::process::Command, HarnessError> {
        let exe = self
            .executable
            .clone()
            .or_else(resolve_pi_executable)
            .ok_or_else(|| {
                HarnessError::NotInstalled(
                    "pi CLI not found. Install with: npm install -g @earendil-works/pi-coding-agent"
                        .into(),
                )
            })?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["--mode", "rpc"]);
        Ok(cmd)
    }

    async fn spawn_pi(
        &self,
        cwd: Option<&str>,
    ) -> Result<(Child, ChildStdin, tokio::io::Lines<BufReader<tokio::process::ChildStdout>>), HarnessError> {
        let mut base = self.pi_command()?;
        let mut cmd = Command::from(base);
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled("pi CLI not found".into())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi has no stdout".into()))?;
        let lines = BufReader::new(stdout).lines();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(target: "zeron_harness::pi::native", stderr = %line);
                }
                tracing::info!(target: "zeron_harness::pi::native", "pi stderr closed");
            });
        }
        Ok((child, stdin, lines))
    }

    /// Send a JSON command to pi's stdin and wait for the response.
    async fn request(
        stdin: &mut ChildStdin,
        lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        cmd: &Value,
    ) -> Result<Value, HarnessError> {
        let id = cmd
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut line = serde_json::to_vec(cmd)
            .map_err(|e| HarnessError::Protocol(e.to_string()))?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        read_response(lines, &id).await
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

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(load_pi_models())
    }

    async fn commands(&self) -> Result<Vec<SlashCommand>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let model = request.model.clone().unwrap_or_default();
        let (provider, model_id) = pi_model_parts(&model);
        let thinking = to_pi_thinking(request.reasoning);

        let (mut child, mut stdin, mut lines) = self.spawn_pi(Some(&request.cwd)).await?;
        let child_pid = child.id();
        tracing::info!(target: "zeron_harness::pi::native", pid = child_pid, cwd = %request.cwd, "pi native session starting");

        // Acknowledge extension_ui_request events that arrive during setup.
        // pi's extensions (dex, todos, etc.) may send requests that need
        // responses to unblock startup.
        // Step 1: get_state to initialize (needed before prompt).
        let state_id = uuid::Uuid::new_v4().to_string();
        Self::request(&mut stdin, &mut lines, &json!({"type": "get_state", "id": state_id})).await?;
        tracing::info!(target: "zeron_harness::pi::native", "get_state ok");

        // Step 2: set model if we have one.
        if !provider.is_empty() && !model_id.is_empty() {
            let req_id = uuid::Uuid::new_v4().to_string();
            Self::request(&mut stdin, &mut lines, &json!({"type": "set_model", "provider": provider, "modelId": model_id, "id": req_id})).await?;
            tracing::info!(target: "zeron_harness::pi::native", provider, model_id, "set_model ok");
        }

        // Step 3: send prompt.
        let prompt_id = uuid::Uuid::new_v4().to_string();
        Self::request(&mut stdin, &mut lines, &json!({"type": "prompt", "message": request.prompt, "id": prompt_id})).await?;
        tracing::info!(target: "zeron_harness::pi::native", "prompt accepted, starting event loop");

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        let interrupt_grace = self.interrupt_grace;
        let kill_grace = self.kill_grace;
        let interrupt_token = controls.interrupt.clone();
        let cwd = request.cwd.clone();

        // Monitor child exit in background.
        tokio::spawn(async move {
            let status = child.wait().await;
            tracing::info!(target: "zeron_harness::pi::native", ?status, "pi process exited");
        });
        tokio::spawn(async move {
            let mut session_id: Option<String> = None;
            let mut sent_started = false;
            let mut interrupted = false;
            tracing::info!(target: "zeron_harness::pi::native", "event loop started");

            loop {
                if interrupt_token.is_cancelled() && !interrupted {
                    interrupted = true;
                    let _ = stdin
                        .write_all(
                            &serde_json::to_vec(&json!({"type": "abort"}))
                                .unwrap_or_default(),
                        )
                        .await;
                    if let Some(pid) = child_pid {
                        let ig = interrupt_grace;
                        let kg = kill_grace;
                        tokio::spawn(async move {
                            tokio::time::sleep(ig).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kg).await;
                            send_signal(pid, Signal::Kill);
                        });
                    }
                }

                let line_result = lines.next_line().await;
                match line_result {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let event: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    target: "zeron_harness::pi::native",
                                    error = %e,
                                    line = %line,
                                    "failed to parse pi event"
                                );
                                continue;
                            }
                        };

                        // Skip response types (they were already read during setup).
                        let ev_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                        if ev_type == "response" {
                            continue;
                        }
                        // Log first 5 events of each type unconditionally to debug
                        if ev_type == "extension_ui_request" {
                            static EXT_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                            let n = EXT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if n < 3 {
                                tracing::info!(target: "zeron_harness::pi::native", %ev_type, n, "event (suppressing further)");
                            }
                        } else {
                            tracing::info!(target: "zeron_harness::pi::native", %ev_type, "event received");
                        }

                        // Emit SessionStarted on first text delta.
                        if !sent_started {
                            if let Some("message_update") = event.get("type").and_then(Value::as_str) {
                                if let Some(ame) = event.get("assistantMessageEvent") {
                                    if ame.get("type").and_then(Value::as_str) == Some("text_delta") {
                                        sent_started = true;
                                        session_id = Some(prompt_id.clone());
                                        let _ = event_tx.send(Ok(AgentEvent::SessionStarted {
                                            harness: HarnessId::Pi,
                                            model: model.clone(),
                                            tools: Vec::new(),
                                            cwd: cwd.clone(),
                                            session_id: prompt_id.clone(),
                                            assistant_message_id: uuid::Uuid::new_v4().to_string(),
                                        })).await;
                                    }
                                }
                            }
                        }

                        let events = translate_pi_event(&event);
                        let is_done = events.iter().any(|ev| matches!(ev, AgentEvent::Done { .. }));
                        if !events.is_empty() {
                            tracing::debug!(target: "zeron_harness::pi::native", count = events.len(), is_done, "translated events");
                        }
                        for ev in events {
                            if event_tx.send(Ok(ev)).await.is_err() {
                                return;
                            }
                        }
                        if is_done && interrupted {
                            return;
                        }
                    }
                    Ok(None) => {
                        tracing::info!(target: "zeron_harness::pi::native", "pi stdout closed");
                        let _ = event_tx.send(Ok(AgentEvent::Done {
                            status: if interrupted {
                                DoneStatus::Interrupted
                            } else {
                                DoneStatus::Completed
                            },
                            result: None,
                            error: None,
                            session_id,
                        })).await;
                        return;
                    }
                    Err(e) => {
                        let _ = event_tx.send(Err(HarnessError::Io(e))).await;
                        return;
                    }
                }
            }
        });

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}
