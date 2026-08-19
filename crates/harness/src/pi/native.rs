//! Native Pi harness: spawns `pi --mode rpc` and speaks its JSONL protocol
//! directly — no adapter process. Mirrors bb's approach of using Pi's SDK,
//! adapted for Rust: we drive pi's RPC mode which is the same protocol
//! `@earendil-works/pi-coding-agent` exposes to the `pi-acp` adapter.
//!
//! Protocol:
//! - stdin:  JSONL commands  (`{"type":"prompt","message":"...","id":"..."}`)
//! - stdout: JSONL events   (`{"type":"text_delta","delta":"..."}`)
//!
//! Event types from pi's RPC mode:
//! - `message_update` with `assistantMessageEvent`:
//!   - `text_delta` / `thinking_delta` → text chunks
//!   - `toolcall_start` / `toolcall_delta` / `toolcall_end` → tool calls
//! - `tool_execution_start` / `tool_execution_update` / `tool_execution_end`
//! - `agent_start` / `turn_end` / `agent_end` / `agent_settled`
//! - `auto_retry_start` / `auto_compaction_start` → status messages
//! - `extension_ui_request` → input requests (select, confirm)

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

/// Locate the pi binary: `PI_EXECUTABLE` env, then PATH, then known
/// install locations (homebrew, npm global).
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

/// Map a pi tool name to a typed [`ToolCall`]. This mirrors the ACP
/// normalizer's `typed_call` but works directly from pi's tool names.
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
        // mcp__server__tool → Mcp
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
/// [`AgentEvent`]s. Returns an empty vec for events we don't translate.
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
                "toolcall_start" | "toolcall_delta" | "toolcall_end" => {
                    let tc = ame
                        .get("toolCall")
                        .or_else(|| {
                            ame.get("partial")
                                .and_then(|p| p.get("content"))
                                .and_then(|c| c.get(ame.get("contentIndex").and_then(Value::as_u64).unwrap_or(0) as usize))
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
                // Try to extract text from the result
                if let Some(text) = r.get("content").and_then(Value::as_str) {
                    Some(text.to_string())
                } else if let Some(text) = r.as_str() {
                    Some(text.to_string())
                } else {
                    // Try common fields
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
        "agent_start" => {
            // Session lifecycle — no user-visible event needed.
            Vec::new()
        }
        "agent_settled" => {
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

/// Parse pi's canonical model id (`provider/modelId`) from Zeron's model
/// selection. The model id from our catalog is already in this format.
fn pi_model_parts(model_id: &str) -> (&str, &str) {
    match model_id.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("", model_id),
    }
}

/// The native Pi harness.
pub struct PiNativeHarness {
    /// Override for the pi binary (tests).
    executable: Option<PathBuf>,
    /// Grace between interrupt and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
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

    /// Override pi binary path (for tests).
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
        cmd.args(["--mode", "rpc", "--no-themes"]);
        Ok(cmd)
    }

    async fn spawn_pi(
        &self,
        cwd: Option<&str>,
    ) -> Result<(Child, ChildStdin), HarnessError> {
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
        // Stderr is logged for debugging.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "zeron_harness::pi::native", stderr = %line);
                }
            });
        }
        Ok((child, stdin))
    }

    /// Send a JSON command to pi's stdin.
    async fn send_command(stdin: &mut ChildStdin, cmd: &Value) -> Result<(), HarnessError> {
        let mut line = serde_json::to_vec(cmd).map_err(|e| HarnessError::Protocol(e.to_string()))?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        Ok(())
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
        // pi supports steering via set_steering_mode RPC
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
        // pi's slash commands are available through the prompt files.
        // For now return an empty list — they're applied client-side
        // by the pi-acp adapter (and would need a get_commands RPC).
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

        let (mut child, mut stdin) = self.spawn_pi(Some(&request.cwd)).await?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi has no stdout".into()))?;

        // Set up the model and thinking level before the prompt.
        if !provider.is_empty() && !model_id.is_empty() {
            Self::send_command(
                &mut stdin,
                &json!({"type": "set_model", "provider": provider, "modelId": model_id}),
            )
            .await?;
        }
        if let Some(level) = thinking {
            Self::send_command(
                &mut stdin,
                &json!({"type": "set_thinking_level", "level": level}),
            )
            .await?;
        }

        // Enable steering so mid-turn prompts work.
        Self::send_command(
            &mut stdin,
            &json!({"type": "set_steering_mode", "mode": "steering"}),
        )
        .await?;

        // Send the prompt.
        let prompt_id = uuid::Uuid::new_v4().to_string();
        Self::send_command(
            &mut stdin,
            &json!({"type": "prompt", "message": request.prompt, "id": prompt_id}),
        )
        .await?;

        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        let interrupt_grace = self.interrupt_grace;
        let kill_grace = self.kill_grace;
        let interrupt_token = controls.interrupt.clone();

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut session_id: Option<String> = None;
            let mut sent_started = false;
            let mut interrupted = false;

            loop {
                if interrupt_token.is_cancelled() && !interrupted {
                    interrupted = true;
                    // Send abort command to pi.
                    let _ = Self::send_command(
                        &mut stdin,
                        &json!({"type": "abort"}),
                    ).await;
                    // Escalate to signals after grace.
                    let pid = child.id();
                    tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        if let Some(pid) = pid {
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }
                    });
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
                                            cwd: request.cwd.clone(),
                                            session_id: prompt_id.clone(),
                                            assistant_message_id: uuid::Uuid::new_v4().to_string(),
                                        })).await;
                                    }
                                }
                            }
                        }

                        let events = translate_pi_event(&event);
                        let is_done = events.iter().any(|ev| matches!(ev, AgentEvent::Done { .. }));
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
                        let _ = event_tx.send(Ok(AgentEvent::Done {
                            status: if interrupted { DoneStatus::Interrupted } else { DoneStatus::Completed },
                            result: None,
                            error: None,
                            session_id: session_id.clone(),
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
