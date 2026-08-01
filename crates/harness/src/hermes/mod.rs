//! Hermes harness: spawns the installed `hermes` CLI as `hermes acp` and speaks
//! the Agent Client Protocol (JSON-RPC 2.0 over stdio) — the same interface
//! Hermes exposes to Zed / VS Code / JetBrains, and the one its own desktop app
//! shares a core with (NousResearch/hermes-agent, `acp_adapter/`).
//!
//! Verified against `hermes acp` from Hermes Agent 0.19.1; every wire shape
//! below (and the fixtures in `normalize`'s tests) was captured from a live
//! session rather than inferred.
//!
//! - `initialize` handshake advertising no client-side fs/terminal capability
//!   (Hermes runs its own file and shell tools; it never calls back), then
//!   `session/new { cwd, mcpServers }` — or `session/load` when resuming.
//! - The session response carries the LIVE model catalog
//!   (`models.availableModels`, provider-qualified ids) and the mode list;
//!   `session/set_model` and `session/set_mode` apply Comet's picks.
//! - `session/prompt` runs one turn and RESOLVES when the turn (plus anything
//!   Hermes queued behind it) is finished — that response, not a notification,
//!   is the authoritative turn end and carries the token usage.
//! - `session/update` notifications map to [`AgentEvent`]s: message/thought
//!   chunks → Text/Reasoning deltas, `tool_call`/`tool_call_update` → typed
//!   ToolCall/ToolResult, `plan` → a Todo call.
//! - Steering: a second `session/prompt` sent while a turn is in flight is
//!   absorbed by Hermes's active-turn redirect (confirmed live: the running
//!   turn changes course mid-stream). Its ack response returns immediately and
//!   is NOT a turn end — the harness tracks in-flight state to tell them apart.
//! - Approvals: `session/request_permission` round-trips through
//!   [`RunControls::request_input`], or is auto-allowed under `auto_approve`.
//! - Interrupt: `session/cancel` (a notification), escalating to SIGTERM →
//!   SIGKILL; the pending prompt resolves with `stopReason: "cancelled"` and
//!   the stream always ends with `Done { status: Interrupted }`.

mod catalog;
mod normalize;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode, ToolCall,
    UserInputAnswer, UserInputQuestion,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls};
use catalog::{
    REASONING_LEVELS, current_model, models_from_session, session_mode, stop_reason_interrupted,
};

/// How long a discovered model catalog stays fresh. Discovery costs a full
/// `hermes acp` spawn plus a throwaway session, so it is worth caching; a few
/// minutes still picks up a `hermes login` in another window without a restart.
const MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Locate the device's installed Hermes CLI: `HERMES_EXECUTABLE`, then PATH,
/// then the locations `setup-hermes.sh` installs into. Resolved per call —
/// cheap, and PATH may be adopted from the login shell after startup.
fn resolve_hermes_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HERMES_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) {
        "hermes.exe"
    } else {
        "hermes"
    };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // The installer's launcher shim, then the venv console script inside
        // HERMES_HOME (`~/.hermes/bin/hermes`, `~/.hermes/hermes-agent/venv/bin`).
        candidates.push(home.join(".local").join("bin").join("hermes"));
        candidates.push(home.join(".hermes").join("bin").join("hermes"));
        candidates.push(
            home.join(".hermes")
                .join("hermes-agent")
                .join("venv")
                .join("bin")
                .join("hermes"),
        );
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/hermes"));
    candidates.push(PathBuf::from("/usr/local/bin/hermes"));
    candidates.into_iter().find(|p| p.exists())
}

/// The Hermes harness. Construct with [`HermesHarness::new`]; tests point it at
/// a fake ACP server with [`HermesHarness::with_executable`].
pub struct HermesHarness {
    executable: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    models: Mutex<Option<(Instant, Vec<Model>)>>,
}

impl Default for HermesHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            models: Mutex::new(None),
        }
    }
}

impl HermesHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_hermes_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "hermes (searched PATH, ~/.local/bin, ~/.hermes/bin, \
                 ~/.hermes/hermes-agent/venv/bin, /opt/homebrew/bin, and /usr/local/bin; \
                 set HERMES_EXECUTABLE to override)"
                    .into(),
            )
        })
    }

    /// Spawn `hermes acp` with piped stdio and a stderr tail reader.
    fn spawn(&self, cwd: Option<&str>) -> Result<(Child, crate::StderrTail), HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("acp");
        crate::prepend_exe_dir_to_path(&mut cmd, &exe);
        if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::hermes", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }
        Ok((child, stderr_tail))
    }

    fn cached_models(&self) -> Option<Vec<Model>> {
        let cache = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .as_ref()
            .filter(|(at, _)| at.elapsed() < MODEL_CACHE_TTL)
            .map(|(_, models)| models.clone())
    }

    fn store_models(&self, models: &[Model]) {
        let mut cache = self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = Some((Instant::now(), models.to_vec()));
    }
}

/// The `initialize` params every connection sends. Hermes never calls back into
/// the client (it owns its file/shell tools), so no fs or terminal capability
/// is advertised — claiming one we don't serve would wedge a turn if a future
/// Hermes started using it.
fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
        "clientInfo": {
            "name": "comet-native",
            "title": "Comet",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

#[async_trait]
impl Harness for HermesHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Hermes
    }
    fn display_name(&self) -> &str {
        // Must match the registry's lazy descriptor so the catalog entry
        // doesn't change after the first resolve.
        "Hermes"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// A prompt sent mid-turn is redirected into the running turn by Hermes's
    /// core (`agent._supports_active_turn_redirect`), landing at the next step
    /// boundary; anything it can't absorb it queues for the next turn.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }

    /// Live catalog: Hermes's models are whatever providers are authenticated
    /// on this device, so a short-lived `hermes acp` reports them via
    /// `session/new`. Cached for [`MODEL_CACHE_TTL`].
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if let Some(models) = self.cached_models() {
            return Ok(models);
        }
        let (mut child, stderr_tail) = self.spawn(None)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("hermes acp child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("hermes acp child has no stdout".into()))?;
        let (client, _incoming) = RpcClient::new(stdin, stdout);

        let discover = async {
            client.request("initialize", initialize_params()).await?;
            let session = client
                .request("session/new", json!({ "cwd": ".", "mcpServers": [] }))
                .await?;
            Ok::<Vec<Model>, HarnessError>(models_from_session(&session))
        };
        let discovered = discover.await;
        shutdown_child(&mut child, self.kill_grace).await;

        match discovered {
            Ok(models) if !models.is_empty() => {
                self.store_models(&models);
                Ok(models)
            }
            // An authenticated-provider-less Hermes reports an empty catalog;
            // surface that as a protocol error with the stderr tail rather than
            // an empty picker with no explanation.
            Ok(_) => Err(HarnessError::Protocol(match stderr_tail.snapshot() {
                Some(tail) => format!("hermes acp reported no models: {tail}"),
                None => {
                    "hermes acp reported no models (run `hermes model` to configure a provider)"
                        .into()
                }
            })),
            Err(e) => Err(e),
        }
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (mut child, stderr_tail) = self.spawn(Some(&request.cwd))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("hermes acp child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("hermes acp child has no stdout".into()))?;

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
}

/// A resolved `session/prompt`. Only [`Prompt::Turn`] ends a turn: a steer sent
/// into a live turn resolves as [`Prompt::Ack`] the moment Hermes absorbs it,
/// while the turn keeps running.
enum Prompt {
    Turn(Result<Value, HarnessError>),
    Ack(Result<Value, HarnessError>),
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// Fire a `session/prompt` without blocking the message loop; the outcome
/// arrives on `tx` tagged by whether it opens a turn or merely acks a steer.
fn spawn_prompt(client: &RpcClient, params: Value, tx: &mpsc::Sender<Prompt>, authoritative: bool) {
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let outcome = client.request("session/prompt", params).await;
        let _ = tx
            .send(if authoritative {
                Prompt::Turn(outcome)
            } else {
                Prompt::Ack(outcome)
            })
            .await;
    });
}

/// Build the `prompt` content blocks: the text turn plus any staged image
/// attachments inlined as base64 (Hermes advertises `promptCapabilities.image`).
async fn prompt_blocks(text: &str, attachments: &[String]) -> Value {
    use base64::Engine as _;
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    for path in attachments {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(target: "comet_harness::hermes", %path, error = %err, "attachment unreadable; path ref only");
                continue;
            }
        };
        if bytes.len() as u64 > crate::claude::MAX_INLINE_IMAGE_BYTES {
            tracing::debug!(target: "comet_harness::hermes", %path, "attachment over inline cap; path ref only");
            continue;
        }
        let Some(mime) = crate::claude::image_media_type(std::path::Path::new(path), &bytes) else {
            tracing::debug!(target: "comet_harness::hermes", %path, "attachment not an inline-supported image; path ref only");
            continue;
        };
        blocks.push(json!({
            "type": "image",
            "mimeType": mime,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }));
    }
    Value::Array(blocks)
}

/// The per-run event loop: one task multiplexing ACP messages, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        client.request("initialize", initialize_params()).await?;

        let session_params = json!({
            "cwd": request.cwd,
            "mcpServers": [],
        });
        let (session_id, result) = match &request.resume {
            Some(resume) => {
                // `session/load` REPLAYS the whole prior transcript as
                // session/update notifications before it responds. Comet's doc
                // already holds those parts, so they are drained and dropped
                // here — concurrently, because a transcript longer than the
                // incoming channel would otherwise block the reader and
                // deadlock the response.
                let load = client.request(
                    "session/load",
                    json!({
                        "sessionId": resume,
                        "cwd": request.cwd,
                        "mcpServers": [],
                    }),
                );
                tokio::pin!(load);
                let loaded = loop {
                    tokio::select! {
                        res = &mut load => break res,
                        inc = incoming.recv() => match inc {
                            // Replay chatter: dropped on the floor.
                            Some(Incoming::Notification { .. }) => continue,
                            // Nothing should ASK us anything mid-replay; refuse
                            // rather than leave the agent waiting forever.
                            Some(Incoming::Request { id, method, .. }) => {
                                client.respond_error(
                                    &id,
                                    -32601,
                                    &format!("unsupported during session/load: {method}"),
                                );
                                continue;
                            }
                            Some(Incoming::Eof) | None => break Err(HarnessError::Protocol(
                                "hermes acp exited during session/load".into(),
                            )),
                        },
                    }
                };
                match loaded {
                    // A `null` result means Hermes has no such session.
                    Ok(result) if !result.is_null() => (resume.clone(), result),
                    other => {
                        if let Err(e) = other {
                            tracing::debug!(
                                target: "comet_harness::hermes",
                                "session/load failed (starting fresh): {e}"
                            );
                        }
                        let result = client
                            .request("session/new", session_params.clone())
                            .await?;
                        let id = result
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        (id, result)
                    }
                }
            }
            None => {
                let result = client.request("session/new", session_params).await?;
                let id = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (id, result)
            }
        };
        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "hermes acp returned no sessionId".into(),
            ));
        }

        // Model + edit-approval mode. A rejected set is logged, not fatal:
        // the session still runs on Hermes's configured defaults.
        if let Some(model) = request.model.as_ref().filter(|m| !m.is_empty())
            && current_model(&result).as_ref() != Some(model)
            && let Err(e) = client
                .request(
                    "session/set_model",
                    json!({ "sessionId": session_id, "modelId": model }),
                )
                .await
        {
            tracing::warn!(
                target: "comet_harness::hermes",
                "session/set_model({model}) rejected; using the session default: {e}"
            );
        }
        let mode = session_mode(request.sandbox, request.auto_approve);
        if let Err(e) = client
            .request(
                "session/set_mode",
                json!({ "sessionId": session_id, "modeId": mode }),
            )
            .await
        {
            tracing::debug!(
                target: "comet_harness::hermes",
                "session/set_mode({mode}) rejected: {e}"
            );
        }
        Ok::<(String, Value), HarnessError>((session_id, result))
    };

    let (session_id, session_result) = tokio::select! {
        res = setup => match res {
            Ok(pair) => pair,
            Err(e) => {
                // A handshake that dies because the child did should say so
                // (missing provider config, a broken venv) rather than shrug.
                let error = match child.try_wait() {
                    Ok(Some(status)) => {
                        crate::crash_message("hermes acp", Some(status), &stderr_tail)
                    }
                    _ => e.to_string(),
                };
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(error),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Hermes,
            model: request
                .model
                .clone()
                .or_else(|| current_model(&session_result))
                .unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // ---- first turn -------------------------------------------------------
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<Prompt>(16);
    let turn_params =
        |blocks: Value| -> Value { json!({ "sessionId": session_id, "prompt": blocks }) };
    spawn_prompt(
        &client,
        turn_params(prompt_blocks(&request.prompt, &request.attachments).await),
        &prompt_tx,
        true,
    );

    // ---- main loop --------------------------------------------------------
    let mut turn_in_flight = true;
    // Steer acks Hermes streams as assistant text, awaiting suppression.
    let mut pending_steer_acks: usize = 0;
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // A Done has been emitted for the turn currently/last in flight.
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    if method != "session/update" {
                        // Unknown notification methods are tolerated by design.
                        continue;
                    }
                    let update = normalize::update(&params).clone();
                    match normalize::update_kind(&params) {
                        "agent_message_chunk" => {
                            if let Some(text) = normalize::chunk_text(&update) {
                                // Drop Hermes's "Redirected the active turn…" /
                                // "Queued for the next turn…" acknowledgement of
                                // OUR steer: Comet renders steering itself.
                                if pending_steer_acks > 0 && normalize::is_steer_ack(&text) {
                                    pending_steer_acks -= 1;
                                    continue;
                                }
                                if !send(&event_tx, AgentEvent::TextDelta { text }).await {
                                    break 'main;
                                }
                            }
                        }

                        "agent_thought_chunk" => {
                            if let Some(text) = normalize::chunk_text(&update)
                                && !send(&event_tx, AgentEvent::ReasoningDelta { text }).await
                            {
                                break 'main;
                            }
                        }

                        "tool_call" => {
                            let id = normalize::tool_call_id(&update);
                            if !send(
                                &event_tx,
                                AgentEvent::ToolCall {
                                    id,
                                    call: normalize::tool_call(&update),
                                },
                            )
                            .await
                            {
                                break 'main;
                            }
                        }

                        "tool_call_update" => {
                            // A terminal update refreshes the call's metadata
                            // and resolves it; progress-only updates are noise.
                            let Some(is_error) = normalize::tool_status(&update) else {
                                continue;
                            };
                            let id = normalize::tool_call_id(&update);
                            if !send(
                                &event_tx,
                                AgentEvent::ToolCall {
                                    id: id.clone(),
                                    call: normalize::tool_call(&update),
                                },
                            )
                            .await
                                || !send(&event_tx, AgentEvent::ToolResult { id, is_error }).await
                            {
                                break 'main;
                            }
                        }

                        "plan" => {
                            // The plan update IS the todo list; give it a stable
                            // id so successive revisions replace one part.
                            let items = normalize::plan_items(&update);
                            if !send(
                                &event_tx,
                                AgentEvent::ToolCall {
                                    id: format!("hermes-plan-{session_id}"),
                                    call: ToolCall::Todo { items },
                                },
                            )
                            .await
                            {
                                break 'main;
                            }
                        }

                        // user_message_chunk (queued-prompt echo),
                        // available_commands_update, usage_update (a context
                        // window gauge, not token counts — the prompt response
                        // carries those), session_info_update, current_mode_update.
                        _ => {}
                    }
                }

                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(
                        &client,
                        id,
                        &method,
                        &params,
                        request.auto_approve,
                        &request_input,
                    );
                }

                // stdout EOF or reader gone: hermes acp exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            prompt = prompt_rx.recv() => match prompt {
                Some(Prompt::Turn(outcome)) => {
                    turn_in_flight = false;
                    match outcome {
                        Ok(result) => {
                            if let Some(usage) = normalize::usage_event(&result)
                                && !send(&event_tx, usage).await
                            {
                                break 'main;
                            }
                            let stop = result
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn");
                            let status = if interrupted || stop_reason_interrupted(stop) {
                                DoneStatus::Interrupted
                            } else {
                                DoneStatus::Completed
                            };
                            done_current = true;
                            if !send(
                                &event_tx,
                                AgentEvent::Done {
                                    status,
                                    result: None,
                                    error: None,
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await
                            {
                                break 'main;
                            }
                            if interrupted {
                                done_after_interrupt = true;
                                break 'main;
                            }
                            // Persistent session: stay alive for the steering
                            // mailbox — the caller owns teardown.
                            if !steering_open {
                                break 'main;
                            }
                        }
                        Err(e) => {
                            done_current = true;
                            if interrupted {
                                done_after_interrupt = true;
                            }
                            let _ = send(
                                &event_tx,
                                AgentEvent::Done {
                                    status: if interrupted {
                                        DoneStatus::Interrupted
                                    } else {
                                        DoneStatus::Errored
                                    },
                                    result: None,
                                    error: Some(e.to_string()),
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await;
                            break 'main;
                        }
                    }
                }
                // A steer absorbed by the live turn: not a turn end. Only a
                // failure is worth surfacing.
                Some(Prompt::Ack(outcome)) => {
                    if let Err(e) = outcome {
                        pending_steer_acks = pending_steer_acks.saturating_sub(1);
                        if !send(
                            &event_tx,
                            AgentEvent::Error {
                                message: format!("Steering failed: {e}"),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    }
                }
                None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let blocks = prompt_blocks(&msg.prompt, &[]).await;
                    // In flight → Hermes redirects it into the running turn and
                    // acks immediately. Idle → this prompt IS the next turn.
                    if turn_in_flight {
                        pending_steer_acks += 1;
                        spawn_prompt(&client, turn_params(blocks), &prompt_tx, false);
                    } else {
                        turn_in_flight = true;
                        done_current = false;
                        spawn_prompt(&client, turn_params(blocks), &prompt_tx, true);
                    }
                    let (prev, next) = rotate(&mut assistant_message_id);
                    if !send(
                        &event_tx,
                        AgentEvent::Steered {
                            assistant_message_id: Some(prev),
                            next_assistant_message_id: Some(next),
                        },
                    )
                    .await
                    {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once nothing is in flight.
                    steering_open = false;
                    if !turn_in_flight {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if turn_in_flight {
                    // `session/cancel` is an ACP notification; the pending
                    // session/prompt then resolves with stopReason "cancelled".
                    client.notify(
                        "session/cancel",
                        Some(json!({ "sessionId": session_id })),
                    );
                    // Escalate if the agent doesn't wind down within the graces.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to interrupt — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child killed mid-turn (OS memory pressure, `killall hermes`)
            // must not read as a silent success.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("hermes acp", status, &stderr_tail)),
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Permissions (approval-as-input parity with comet's UX)
// ---------------------------------------------------------------------------

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Pick the option id matching an outcome. ACP option `kind`s are
/// `allow_once` / `allow_always` / `reject_once` / `reject_always`; the id is
/// free-form, so selection goes by kind with an id-prefix fallback.
fn option_id(params: &Value, allow: bool) -> Option<String> {
    let options = params.get("options").and_then(Value::as_array)?;
    let wanted = if allow { "allow" } else { "reject" };
    options
        .iter()
        .find(|o| {
            o.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| k.starts_with(wanted))
        })
        .or_else(|| {
            options.iter().find(|o| {
                o.get("optionId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with(wanted) || (!allow && id.starts_with("deny")))
            })
        })
        .and_then(|o| o.get("optionId").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Serve one server→client request. `session/request_permission` round-trips
/// through `request_input` as a synthesized yes/no question (in a subtask so
/// the message loop keeps flowing); with `auto_approve` it is allowed outright.
/// Anything else is rejected as unsupported so the agent never wedges awaiting
/// a reply.
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    auto_approve: bool,
    request_input: &Arc<RequestInputFn>,
) {
    if method != "session/request_permission" {
        tracing::debug!(
            target: "comet_harness::hermes",
            "unhandled server request: {method}"
        );
        client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
        return;
    }

    let respond = |client: &RpcClient, id: &Value, allow: bool| match option_id(params, allow) {
        Some(option) => client.respond(
            &id.clone(),
            json!({ "outcome": { "outcome": "selected", "optionId": option } }),
        ),
        // No option of the wanted polarity: cancel rather than guess.
        None => client.respond(
            &id.clone(),
            json!({ "outcome": { "outcome": "cancelled" } }),
        ),
    };

    if auto_approve {
        respond(client, &id, true);
        return;
    }

    let question = permission_question(params);
    let client = client.clone();
    let params = params.clone();
    let request_input = Arc::clone(request_input);
    tokio::spawn(async move {
        // The engine's input bridge owns the InputRequested/InputResolved
        // lifecycle; a dropped sender (caller went away) degrades to a decline
        // so the agent is unblocked — never silently allowed.
        let answers = (request_input)(vec![question.clone()])
            .await
            .unwrap_or_default();
        let accept = answers.iter().any(|a| {
            a.question_id == question.id && a.labels.iter().any(|l| l.eq_ignore_ascii_case("yes"))
        });
        match option_id(&params, accept) {
            Some(option) => client.respond(
                &id,
                json!({ "outcome": { "outcome": "selected", "optionId": option } }),
            ),
            None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
        }
    });
}

/// Synthesize the yes/no question a permission request surfaces to the user.
/// Hermes puts the pending call under `toolCall` (title, kind, and — uniquely
/// for permission prompts — a `rawInput` naming the tool and its arguments).
fn permission_question(params: &Value) -> UserInputQuestion {
    let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
    let title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let kind = tool_call.get("kind").and_then(Value::as_str).unwrap_or("");
    let header = match kind {
        "execute" => "Approve command",
        "edit" => "Approve file change",
        "fetch" => "Approve network access",
        _ => "Approve tool call",
    };
    let question = if title.is_empty() {
        "Hermes wants to run a tool. Allow it?".to_owned()
    } else {
        format!("Hermes wants to `{title}`. Allow it?")
    };
    UserInputQuestion {
        id: new_message_id(),
        header: header.to_owned(),
        question,
        options: vec!["Yes".into(), "No".into()],
        multi_select: false,
    }
}

// ---------------------------------------------------------------------------
// Child lifecycle
// ---------------------------------------------------------------------------

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
async fn shutdown_child(child: &mut Child, kill_grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Options captured from a live `session/request_permission`.
    #[test]
    fn permission_options_select_by_kind() {
        let params = json!({
            "options": [
                {"kind": "allow_once", "name": "Allow edit", "optionId": "allow_once"},
                {"kind": "reject_once", "name": "Deny", "optionId": "deny"},
            ],
        });
        assert_eq!(option_id(&params, true).as_deref(), Some("allow_once"));
        assert_eq!(option_id(&params, false).as_deref(), Some("deny"));
    }

    /// Kinds are the contract, but a server that omits them still resolves via
    /// the id prefix (including Hermes's "deny").
    #[test]
    fn permission_options_fall_back_to_id_prefix() {
        let params = json!({
            "options": [
                {"name": "Allow", "optionId": "allow_always"},
                {"name": "Deny", "optionId": "deny"},
            ],
        });
        assert_eq!(option_id(&params, true).as_deref(), Some("allow_always"));
        assert_eq!(option_id(&params, false).as_deref(), Some("deny"));
        // Nothing matching either polarity → cancel, never a guess.
        assert_eq!(option_id(&json!({"options": []}), true), None);
        assert_eq!(option_id(&json!({}), false), None);
    }

    #[test]
    fn permission_questions_are_yes_no_and_name_the_call() {
        let q = permission_question(&json!({
            "toolCall": {"kind": "edit", "title": "Approve edit: notes.txt"},
        }));
        assert_eq!(q.header, "Approve file change");
        assert!(q.question.contains("Approve edit: notes.txt"));
        assert_eq!(q.options, vec!["Yes".to_string(), "No".to_string()]);
        assert!(!q.multi_select);

        let q = permission_question(&json!({"toolCall": {"kind": "execute", "title": "rm -rf /"}}));
        assert_eq!(q.header, "Approve command");

        // A titleless call still asks something answerable.
        let q = permission_question(&json!({}));
        assert_eq!(q.header, "Approve tool call");
        assert!(q.question.contains("Allow it?"));
    }

    #[tokio::test]
    async fn prompt_blocks_carry_text_and_skip_unreadable_attachments() {
        let blocks = prompt_blocks("hello", &["/nonexistent/x.png".into()]).await;
        let blocks = blocks.as_array().unwrap();
        assert_eq!(blocks.len(), 1, "unreadable attachments are skipped");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");

        // A real PNG is inlined as an ACP image block.
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let blocks = prompt_blocks("look", &[png.display().to_string()]).await;
        let blocks = blocks.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert!(blocks[1]["data"].as_str().is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn initialize_advertises_no_client_side_filesystem() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], false);
        assert_eq!(params["clientInfo"]["name"], "comet-native");
    }
}
