//! OpenCode subagent visualization on the ACP path.
//!
//! opencode's `task` tool call rides the parent's `session/update` stream
//! (kind `think`, `rawInput: {description, prompt, subagent_type}`), but the
//! child session's interior transcript never appears on the ACP wire — the
//! parent only sees the completion's `<task_result>` text. The transcript IS
//! available live from the process itself: `opencode acp` always doubles as
//! opencode's HTTP server (zeron passes `--port <free>` to dodge the shared
//! default 4096, where a losing concurrent bind silently drops the server),
//! and its `/event` SSE bus broadcasts every session's `session.created` /
//! `message.updated` / `message.part.updated` / `message.part.delta` events —
//! child sessions included, token-level (verified live, 1.18.18; storage
//! moved to SQLite in 1.18, so there is no JSONL to tail grok-style).
//!
//! Correlation: a `task` tool call registers a pending chip; the bus's
//! `session.created` for a child (`parentID` = the parent ACP session, title
//! `"{description} (@{agent} subagent)"`) binds by description, else FIFO —
//! and the task completion's `rawOutput.metadata.sessionId` is the
//! authoritative late binding. The bound child's bus traffic maps to tagged
//! [`AgentEvent::Subagent`] events; the ACP task completion settles the chip
//! with a tagged Done (`task` runs synchronously — the child always finishes
//! before its chip does). The bus is vendor-private: every parse fails soft,
//! degrading to chip + final `<task_result>` output.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use zeron_proto::{AgentEvent, DoneStatus, TodoItem, ToolCall};

use crate::HarnessError;

use super::normalize::{OUTPUT_CAP, cap_text};

/// Budget on the first `/event` connect: the server binds a few seconds into
/// the process's life, well inside this window.
const CONNECT_ATTEMPTS: u32 = 120;
const CONNECT_POLL: Duration = Duration::from_millis(250);
/// Grace between the ACP task completion and the tagged Done, letting the
/// child's last bus frames (an independent channel) land in order.
const SETTLE_DRAIN: Duration = Duration::from_millis(400);

/// Wrap an event as subagent-attributed traffic.
fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: None,
    }
}

/// A `task` chip seen on the ACP wire, awaiting a child session.
struct PendingSpawn {
    tool_call_id: String,
    description: String,
}

/// A bus `session.created` that arrived before any chip could be bound
/// (defensive: the chip's rawInput update precedes tool execution today).
struct UnboundChild {
    title: String,
}

/// What a bound child's bus traffic has produced so far.
#[derive(Default)]
struct ChildState {
    /// Text parts whose message ROLE is not yet known (`message.part.updated`
    /// raced ahead of `message.updated`): replayed when the role lands.
    pending_parts: Vec<Value>,
    /// The spawn chip this child streams to.
    parent_tool_use_id: String,
    /// messageID → is-assistant (user prompt echoes must not render).
    assistant_messages: HashMap<String, bool>,
    /// partID → streaming state (dedup between snapshots and deltas).
    parts: HashMap<String, PartState>,
    /// Any transcript reached the doc — the completion fallback text would
    /// only duplicate it.
    saw_transcript: bool,
    /// Chip settled (tagged Done sent or scheduled); late traffic drops.
    done: bool,
}

#[derive(Default)]
struct PartState {
    /// "text" | "reasoning" | "tool" (unknown kinds are never registered).
    kind: String,
    /// Bytes of part text already emitted (snapshots resend the full text;
    /// deltas append).
    emitted: usize,
    tool_started: bool,
    tool_done: bool,
}

struct OcState {
    /// The parent ACP session — a nested spawn's `session.created` (child of
    /// a child) must not bind to this feed's chips.
    session_id: String,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pending: VecDeque<PendingSpawn>,
    /// child session id → streaming state.
    children: HashMap<String, ChildState>,
    unbound: HashMap<String, UnboundChild>,
    /// Tracker dropped: the bus task exits and late completions settle
    /// nothing further.
    torn_down: bool,
}

pub(crate) struct OpencodeTracker {
    state: Arc<Mutex<OcState>>,
}

impl OpencodeTracker {
    /// `sidecar_base` is the process's HTTP root (`http://127.0.0.1:{port}`);
    /// `None` (the port pick failed) degrades to chip + final output.
    pub(crate) fn new(
        session_id: String,
        event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
        sidecar_base: Option<String>,
    ) -> Self {
        let state = Arc::new(Mutex::new(OcState {
            session_id,
            event_tx,
            pending: VecDeque::new(),
            children: HashMap::new(),
            unbound: HashMap::new(),
            torn_down: false,
        }));
        if let Some(base) = sidecar_base {
            tokio::spawn(bus_task(Arc::clone(&state), base));
        }
        Self { state }
    }

    /// Inspect one `session/update` payload's `update` object (ACP side).
    pub(crate) fn observe(&mut self, update: &Value) {
        if !matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("tool_call") | Some("tool_call_update")
        ) {
            return;
        }
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("tracker lock");
        // A task spawn is identified by its rawInput shape — the tool name is
        // only ever a display title on this wire. The first `pending` frame
        // carries an empty rawInput and is skipped; the in_progress update
        // (full rawInput) lands before the tool runs, so the chip is always
        // registered ahead of the child's `session.created`.
        if let Some(raw) = update.get("rawInput")
            && raw.get("subagent_type").is_some()
            && raw.get("prompt").is_some()
        {
            let description = raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let known = state.pending.iter().any(|p| p.tool_call_id == id)
                || state.children.values().any(|c| c.parent_tool_use_id == id);
            if !known {
                state.pending.push_back(PendingSpawn {
                    tool_call_id: id.to_owned(),
                    description: description.to_owned(),
                });
                // A child that raced ahead of its chip binds now.
                if let Some(child_id) = match_unbound(&state.unbound, description) {
                    state.unbound.remove(&child_id);
                    bind(&mut state, &child_id);
                }
            }
        }
        let status = update.get("status").and_then(Value::as_str);
        if matches!(status, Some("completed") | Some("failed")) {
            self.settle_from_completion(&mut state, id, update, status == Some("failed"));
        }
    }

    /// A task chip completed on the ACP wire: bind by the completion's
    /// authoritative child session id, then settle the chip with a tagged
    /// Done — after a short drain when the bus streamed (its last frames ride
    /// an independent channel), immediately with the `<task_result>` fallback
    /// text when it never did.
    fn settle_from_completion(
        &self,
        state: &mut OcState,
        tool_call_id: &str,
        update: &Value,
        failed: bool,
    ) {
        let known_chip = state.pending.iter().any(|p| p.tool_call_id == tool_call_id)
            || state
                .children
                .values()
                .any(|c| c.parent_tool_use_id == tool_call_id);
        if !known_chip {
            return;
        }
        let child_id = completion_child_session(update);
        state.pending.retain(|p| p.tool_call_id != tool_call_id);
        let child_id = match child_id {
            Some(id) => {
                state.unbound.remove(&id);
                let entry = state.children.entry(id.clone()).or_default();
                // First completion binds; a RESUME tool call's completion
                // must not re-key an already-bound child — its transcript
                // continues under the ORIGINAL spawn chip.
                if entry.parent_tool_use_id.is_empty() {
                    entry.parent_tool_use_id = tool_call_id.to_owned();
                }
                id
            }
            // No id on the completion (older wire?): settle whichever bound
            // child streams to this chip, else a synthetic record.
            None => state
                .children
                .iter()
                .find(|(_, c)| c.parent_tool_use_id == tool_call_id)
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| format!("unbound:{tool_call_id}")),
        };
        let child = state
            .children
            .entry(child_id)
            .or_insert_with(|| ChildState {
                parent_tool_use_id: tool_call_id.to_owned(),
                ..ChildState::default()
            });
        if child.done {
            return;
        }
        child.done = true;
        let status = if failed {
            DoneStatus::Errored
        } else {
            DoneStatus::Completed
        };
        let fallback = (!child.saw_transcript)
            .then(|| completion_result_text(update))
            .flatten();
        let parent = child.parent_tool_use_id.clone();
        let drain = child.saw_transcript;
        let event_tx = state.event_tx.clone();
        tokio::spawn(async move {
            if drain {
                tokio::time::sleep(SETTLE_DRAIN).await;
            }
            if let Some(text) = fallback {
                let _ = event_tx
                    .send(Ok(tag(&parent, AgentEvent::TextDelta { text })))
                    .await;
            }
            let _ = event_tx.send(Ok(tag(&parent, done(status)))).await;
        });
    }
}

impl Drop for OpencodeTracker {
    /// Session teardown with subagents still streaming: settle every open
    /// chip as interrupted rather than leaving it spinning forever. The
    /// sends ride spawned tasks (whose `event_tx` clones keep the run's
    /// event stream open until they land, grok-tail parity).
    fn drop(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.torn_down = true;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let sender = state.event_tx.clone();
        for child in state.children.values_mut() {
            if child.done {
                continue;
            }
            child.done = true;
            let parent = child.parent_tool_use_id.clone();
            let event_tx = sender.clone();
            handle.spawn(async move {
                let _ = event_tx
                    .send(Ok(tag(&parent, done(DoneStatus::Interrupted))))
                    .await;
            });
        }
    }
}

/// The completion's child session id: first-class
/// `rawOutput.metadata.sessionId`, else parsed from the output's
/// `<task id="...">` wrapper.
fn completion_child_session(update: &Value) -> Option<String> {
    let metadata = update.get("rawOutput").and_then(|r| r.get("metadata"));
    if let Some(id) = metadata
        .and_then(|m| m.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(id.to_owned());
    }
    let text = completion_output(update)?;
    let after = text.split("<task id=\"").nth(1)?;
    let id = after.split('"').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// The completion's raw output text (`rawOutput.output`, else the content
/// blocks).
fn completion_output(update: &Value) -> Option<String> {
    if let Some(text) = update
        .get("rawOutput")
        .and_then(|r| r.get("output"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    let parts: Vec<&str> = update
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|c| {
            let block = c.get("content")?;
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The `<task_result>` body of a completion's output — the whole text when
/// the wrapper is missing (fail soft on a vendor-private shape).
fn completion_result_text(update: &Value) -> Option<String> {
    let text = completion_output(update)?;
    let body = text
        .split("<task_result>")
        .nth(1)
        .and_then(|t| t.split("</task_result>").next())
        .unwrap_or(&text)
        .trim();
    (!body.is_empty()).then(|| cap_text(body, OUTPUT_CAP))
}

/// The unbound child whose title matches a pending description, else the
/// only one (FIFO would need arrival order; one entry is the common case).
fn match_unbound(unbound: &HashMap<String, UnboundChild>, description: &str) -> Option<String> {
    if !description.is_empty()
        && let Some(id) = unbound
            .iter()
            .find(|(_, u)| u.title.starts_with(description))
            .map(|(id, _)| id.clone())
    {
        return Some(id);
    }
    (unbound.len() == 1)
        .then(|| unbound.keys().next().cloned())
        .flatten()
}

/// Bind a stashed/incoming child to a pending chip: description match
/// against the child title (`"{description} (@{agent} subagent)"`, FIFO
/// across identical descriptions), else the oldest pending spawn.
fn bind_to_pending(state: &mut OcState, child_id: &str, title: &str) -> bool {
    let ix = state
        .pending
        .iter()
        .position(|p| !p.description.is_empty() && title.starts_with(&p.description))
        .or(if state.pending.is_empty() {
            None
        } else {
            Some(0)
        });
    match ix.and_then(|i| state.pending.remove(i)) {
        Some(p) => {
            state.children.insert(
                child_id.to_owned(),
                ChildState {
                    parent_tool_use_id: p.tool_call_id,
                    ..ChildState::default()
                },
            );
            true
        }
        None => false,
    }
}

/// Bind a previously-stashed unbound child (title already consumed).
fn bind(state: &mut OcState, child_id: &str) {
    if let Some(p) = state.pending.pop_front() {
        state.children.insert(
            child_id.to_owned(),
            ChildState {
                parent_tool_use_id: p.tool_call_id,
                ..ChildState::default()
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Bus (SSE) side
// ---------------------------------------------------------------------------

/// Tail the sidecar's `/event` SSE bus into tagged events. Retries the first
/// connect (the server binds a few seconds into the process's life); an
/// established stream ending means the process is exiting — no reconnect,
/// the ACP completion fallback still settles every chip.
async fn bus_task(state: Arc<Mutex<OcState>>, base: String) {
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
    else {
        return;
    };
    let url = format!("{base}/event");
    for _ in 0..CONNECT_ATTEMPTS {
        tokio::time::sleep(CONNECT_POLL).await;
        if closed(&state) {
            return;
        }
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        stream_events(&state, resp).await;
        return;
    }
    tracing::debug!(
        target: "zeron_harness::acp",
        "opencode sidecar bus never connected ({base}); subagent transcripts degrade to final output"
    );
}

async fn stream_events(state: &Arc<Mutex<OcState>>, resp: reqwest::Response) {
    use futures::StreamExt as _;
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            return;
        };
        buf.extend_from_slice(&bytes);
        // SSE frames are blank-line separated; each data line is one event.
        while let Some(pos) = find_frame_end(&buf) {
            let frame: Vec<u8> = buf.drain(..pos + 2).collect();
            let Ok(frame) = std::str::from_utf8(&frame) else {
                continue;
            };
            for line in frame.lines() {
                let Some(data) = line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let (event_tx, tagged) = {
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    if state.torn_down {
                        return;
                    }
                    (state.event_tx.clone(), handle_bus_event(&mut state, &event))
                };
                for ev in tagged {
                    if event_tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            }
        }
        if closed(state) {
            return;
        }
    }
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

fn closed(state: &Arc<Mutex<OcState>>) -> bool {
    state
        .lock()
        .map(|s| s.torn_down || s.event_tx.is_closed())
        .unwrap_or(true)
}

/// Map one bus event to tagged transcript events (pure bookkeeping +
/// mapping; the caller sends outside the lock).
fn handle_bus_event(state: &mut OcState, event: &Value) -> Vec<AgentEvent> {
    let props = event.get("properties").unwrap_or(&Value::Null);
    match event.get("type").and_then(Value::as_str) {
        Some("session.created") => {
            let info = props.get("info").unwrap_or(&Value::Null);
            // A nested spawn (a subagent's own subagent) carries the CHILD's
            // session as parentID — never bind it to this feed's chips.
            if info.get("parentID").and_then(Value::as_str) != Some(state.session_id.as_str()) {
                return Vec::new();
            }
            let Some(child_id) = info
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return Vec::new();
            };
            if state.children.contains_key(child_id) {
                return Vec::new();
            }
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !bind_to_pending(state, child_id, title) {
                state.unbound.insert(
                    child_id.to_owned(),
                    UnboundChild {
                        title: title.to_owned(),
                    },
                );
            }
            Vec::new()
        }
        Some("message.updated") => {
            let info = props.get("info").unwrap_or(&Value::Null);
            let (Some(session), Some(message), Some(role)) = (
                info.get("sessionID").and_then(Value::as_str),
                info.get("id").and_then(Value::as_str),
                info.get("role").and_then(Value::as_str),
            ) else {
                return Vec::new();
            };
            if let Some(child) = state.children.get_mut(session) {
                child
                    .assistant_messages
                    .entry(message.to_owned())
                    .or_insert(role == "assistant");
                // A NEW user message on a settled child is a steer resuming
                // it (opencode re-prompts the same session): un-latch so the
                // resumed traffic streams to the same chip again.
                if role == "user" && child.done {
                    child.done = false;
                }
                // Parts that raced ahead of this role fact replay now.
                let held: Vec<Value> = std::mem::take(&mut child.pending_parts)
                    .into_iter()
                    .filter(|part| {
                        part.get("messageID").and_then(Value::as_str) == Some(message)
                    })
                    .collect();
                if !held.is_empty() {
                    let parent = child.parent_tool_use_id.clone();
                    return held
                        .iter()
                        .flat_map(|part| part_snapshot_events(child, part))
                        .map(|ev| tag(&parent, ev))
                        .collect();
                }
            }
            Vec::new()
        }
        Some("message.part.updated") => {
            let part = props.get("part").unwrap_or(&Value::Null);
            let Some(session) = part.get("sessionID").and_then(Value::as_str) else {
                return Vec::new();
            };
            let Some(child) = state.children.get_mut(session) else {
                return Vec::new();
            };
            let parent = child.parent_tool_use_id.clone();
            part_snapshot_events(child, part)
                .into_iter()
                .map(|ev| tag(&parent, ev))
                .collect()
        }
        Some("message.part.delta") => {
            let (Some(session), Some(part_id), Some(delta)) = (
                props.get("sessionID").and_then(Value::as_str),
                props.get("partID").and_then(Value::as_str),
                props.get("delta").and_then(Value::as_str),
            ) else {
                return Vec::new();
            };
            if props.get("field").and_then(Value::as_str) != Some("text") {
                return Vec::new();
            }
            let Some(child) = state.children.get_mut(session) else {
                return Vec::new();
            };
            let parent = child.parent_tool_use_id.clone();
            part_delta_events(child, props, part_id, delta)
                .into_iter()
                .map(|ev| tag(&parent, ev))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// A part snapshot: emit whatever text extends what already streamed, open /
/// resolve tool chips. Snapshots and deltas interleave — `emitted` (bytes of
/// part text already sent) is the dedup line between them.
fn part_snapshot_events(child: &mut ChildState, part: &Value) -> Vec<AgentEvent> {
    if child.done {
        return Vec::new();
    }
    let Some(part_id) = part.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let message_id = part
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" | "reasoning" => {
            // A user-role text part is the message INTO the child — its
            // spawn prompt (and any future steer). opencode's own UI renders
            // these, so we do too: one UserMessage per part (posted
            // atomically — there is no user delta channel), which the engine
            // writes as its own user entry.
            if kind == "text" && child.assistant_messages.get(message_id).is_none() {
                // Role unknown: hold the part instead of guessing (dedup by
                // part id — snapshots re-deliver).
                if !child
                    .pending_parts
                    .iter()
                    .any(|p| p.get("id").and_then(Value::as_str) == Some(part_id))
                {
                    child.pending_parts.push(part.clone());
                }
                return Vec::new();
            }
            if kind == "text" && child.assistant_messages.get(message_id) == Some(&false) {
                let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                if text.trim().is_empty() {
                    return Vec::new();
                }
                let entry = child
                    .parts
                    .entry(part_id.to_owned())
                    .or_insert_with(|| PartState {
                        kind: kind.to_owned(),
                        ..PartState::default()
                    });
                if entry.emitted > 0 {
                    return Vec::new();
                }
                entry.emitted = text.len();
                child.saw_transcript = true;
                return vec![AgentEvent::UserMessage {
                    text: text.to_owned(),
                }];
            }
            if child.assistant_messages.get(message_id) != Some(&true) {
                return Vec::new();
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            let entry = child
                .parts
                .entry(part_id.to_owned())
                .or_insert_with(|| PartState {
                    kind: kind.to_owned(),
                    ..PartState::default()
                });
            let Some(suffix) = text
                .get(entry.emitted..)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
            else {
                // Shorter (or mid-char) snapshot: a rewrite this tracker
                // doesn't model — drop it rather than duplicate text.
                return Vec::new();
            };
            entry.emitted = text.len();
            child.saw_transcript = true;
            vec![if entry.kind == "reasoning" {
                AgentEvent::ReasoningDelta { text: suffix }
            } else {
                AgentEvent::TextDelta { text: suffix }
            }]
        }
        "tool" => {
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or_default();
            let status = part
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = part
                .get("state")
                .and_then(|s| s.get("input"))
                .cloned()
                .unwrap_or(Value::Null);
            let call_id = part
                .get("callID")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(part_id)
                .to_owned();
            let entry = child
                .parts
                .entry(part_id.to_owned())
                .or_insert_with(|| PartState {
                    kind: "tool".to_owned(),
                    ..PartState::default()
                });
            let mut events = Vec::new();
            let has_input = input.as_object().is_some_and(|o| !o.is_empty());
            if !entry.tool_started && (has_input || matches!(status, "completed" | "error")) {
                entry.tool_started = true;
                child.saw_transcript = true;
                events.push(AgentEvent::ToolCall {
                    id: call_id.clone(),
                    call: oc_tool_call(tool, &input),
                });
            }
            if entry.tool_started && !entry.tool_done && matches!(status, "completed" | "error") {
                entry.tool_done = true;
                let output = part
                    .get("state")
                    .and_then(|s| {
                        s.get("output")
                            .or_else(|| s.get("error"))
                            .and_then(Value::as_str)
                    })
                    .filter(|t| !t.is_empty())
                    .map(|t| cap_text(t, OUTPUT_CAP));
                events.push(AgentEvent::ToolResult {
                    id: call_id,
                    is_error: status == "error",
                    output,
                    diff: None,
                });
            }
            events
        }
        // step-start / step-finish / snapshot bookkeeping: not transcript.
        _ => Vec::new(),
    }
}

/// A text delta appends to its part. Deltas follow the part's opening
/// `message.part.updated` (which fixes the kind); an unknown part defaults
/// to assistant text only when its message is known assistant.
fn part_delta_events(
    child: &mut ChildState,
    props: &Value,
    part_id: &str,
    delta: &str,
) -> Vec<AgentEvent> {
    if child.done || delta.is_empty() {
        return Vec::new();
    }
    let message_id = props
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if child.assistant_messages.get(message_id) != Some(&true) {
        return Vec::new();
    }
    let entry = child
        .parts
        .entry(part_id.to_owned())
        .or_insert_with(|| PartState {
            kind: "text".to_owned(),
            ..PartState::default()
        });
    if entry.kind == "tool" {
        return Vec::new();
    }
    entry.emitted += delta.len();
    child.saw_transcript = true;
    vec![if entry.kind == "reasoning" {
        AgentEvent::ReasoningDelta {
            text: delta.to_owned(),
        }
    } else {
        AgentEvent::TextDelta {
            text: delta.to_owned(),
        }
    }]
}

/// Type an opencode-native tool invocation (bus names, not ACP kinds).
fn oc_tool_call(name: &str, input: &Value) -> ToolCall {
    let s = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    match name {
        "bash" => ToolCall::Exec {
            command: s(&["command"]).unwrap_or_default(),
        },
        "read" => ToolCall::ReadFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
        },
        "write" => ToolCall::WriteFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
            content: s(&["content"]),
        },
        "edit" => ToolCall::EditFile {
            path: s(&["filePath", "file_path", "path"]).unwrap_or_default(),
            old_string: s(&["oldString", "old_string"]),
            new_string: s(&["newString", "new_string"]),
        },
        "patch" => ToolCall::ApplyPatch {
            path: s(&["filePath", "file_path", "path"]),
        },
        "grep" => ToolCall::Search {
            pattern: s(&["pattern"]).unwrap_or_default(),
            path: s(&["path", "include"]),
        },
        "glob" => ToolCall::Glob {
            pattern: s(&["pattern"]).unwrap_or_default(),
        },
        "webfetch" => ToolCall::WebFetch {
            url: s(&["url"]).unwrap_or_default(),
            prompt: None,
        },
        "websearch" => ToolCall::WebSearch {
            query: s(&["query"]).unwrap_or_default(),
        },
        "todowrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: t
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // A nested spawn inside the subagent: same naming as the parent chip
        // (no recursive viz — it renders as a plain chip in the child doc).
        "task" => ToolCall::Unknown {
            name: s(&["description"])
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: (!input.is_null()).then(|| input.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: (!input.is_null()).then(|| input.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tracker() -> (
        OpencodeTracker,
        mpsc::Receiver<Result<AgentEvent, HarnessError>>,
    ) {
        let (tx, rx) = mpsc::channel(64);
        (OpencodeTracker::new("ses_parent".into(), tx, None), rx)
    }

    fn task_update(id: &str, status: &str, raw_output: Option<Value>) -> Value {
        let mut update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": status,
            "kind": "think",
            "title": "Viz probe",
            "rawInput": {
                "description": "Viz probe",
                "prompt": "run the probe",
                "subagent_type": "general",
            },
        });
        if let Some(raw) = raw_output {
            update["rawOutput"] = raw;
        }
        update
    }

    #[test]
    fn completion_metadata_yields_the_child_session() {
        let update = task_update(
            "t1",
            "completed",
            Some(json!({
                "output": "<task id=\"ses_child\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"parentSessionId": "ses_parent", "sessionId": "ses_child"},
            })),
        );
        assert_eq!(
            completion_child_session(&update).as_deref(),
            Some("ses_child")
        );
        assert_eq!(completion_result_text(&update).as_deref(), Some("finished"));
        // metadata absent → the <task id> wrapper still yields the id.
        let update = task_update(
            "t1",
            "completed",
            Some(json!({
                "output": "<task id=\"ses_x\" state=\"completed\">\n<task_result>\nok\n</task_result>\n</task>",
            })),
        );
        assert_eq!(completion_child_session(&update).as_deref(), Some("ses_x"));
    }

    #[tokio::test]
    async fn unstreamed_child_settles_with_fallback_text_and_done() {
        let (mut tracker, mut rx) = tracker();
        tracker.observe(&task_update("t1", "in_progress", None));
        tracker.observe(&task_update(
            "t1",
            "completed",
            Some(json!({
                "output": "<task id=\"ses_child\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"sessionId": "ses_child"},
            })),
        ));
        let ev = rx.recv().await.expect("event").expect("ok");
        assert!(matches!(
            &ev,
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "t1"
                    && matches!(&**event, AgentEvent::TextDelta { text } if text == "finished")
        ));
        let ev = rx.recv().await.expect("event").expect("ok");
        assert!(matches!(
            &ev,
            AgentEvent::Subagent { event, .. }
                if matches!(&**event, AgentEvent::Done { status: DoneStatus::Completed, .. })
        ));
    }

    #[tokio::test]
    async fn bus_traffic_streams_tagged_and_completion_settles() {
        let (mut tracker, mut rx) = tracker();
        tracker.observe(&task_update("t1", "in_progress", None));
        {
            let mut state = tracker.state.lock().unwrap();
            // Real 1.18.18 bus shapes.
            let created = json!({"type": "session.created", "properties": {"info": {
                "id": "ses_child", "parentID": "ses_parent",
                "title": "Viz probe (@general subagent)", "agent": "general",
            }}});
            assert!(handle_bus_event(&mut state, &created).is_empty());
            assert!(state.children.contains_key("ses_child"));

            let user_msg = json!({"type": "message.updated", "properties": {"info": {
                "id": "msg_u", "role": "user", "sessionID": "ses_child",
            }}});
            handle_bus_event(&mut state, &user_msg);
            let user_part = json!({"type": "message.part.updated", "properties": {"part": {
                "id": "prt_u", "messageID": "msg_u", "sessionID": "ses_child",
                "type": "text", "text": "run the probe",
            }}});
            // The message INTO the child (its prompt / a steer) forwards as
            // a UserMessage — once: a re-delivered snapshot must not double
            // the entry.
            assert!(matches!(
                handle_bus_event(&mut state, &user_part).as_slice(),
                [AgentEvent::Subagent { event, .. }]
                    if matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "run the probe")
            ));
            assert!(handle_bus_event(&mut state, &user_part).is_empty());

            let asst_msg = json!({"type": "message.updated", "properties": {"info": {
                "id": "msg_a", "role": "assistant", "sessionID": "ses_child",
            }}});
            handle_bus_event(&mut state, &asst_msg);
            let tool_running = json!({"type": "message.part.updated", "properties": {"part": {
                "id": "prt_t", "messageID": "msg_a", "sessionID": "ses_child",
                "type": "tool", "tool": "bash", "callID": "call-1",
                "state": {"status": "running", "input": {"command": "echo ok"}},
            }}});
            let events = handle_bus_event(&mut state, &tool_running);
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::Subagent { parent_tool_use_id, event }]
                    if parent_tool_use_id == "t1"
                        && matches!(&**event, AgentEvent::ToolCall { id, call: ToolCall::Exec { command } }
                            if id == "call-1" && command == "echo ok")
            ));
            // Repeated running snapshots don't re-open the chip.
            assert!(handle_bus_event(&mut state, &tool_running).is_empty());
            let tool_done = json!({"type": "message.part.updated", "properties": {"part": {
                "id": "prt_t", "messageID": "msg_a", "sessionID": "ses_child",
                "type": "tool", "tool": "bash", "callID": "call-1",
                "state": {"status": "completed", "input": {"command": "echo ok"}, "output": "ok\n"},
            }}});
            let events = handle_bus_event(&mut state, &tool_done);
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::Subagent { event, .. }]
                    if matches!(&**event, AgentEvent::ToolResult { id, is_error: false, output: Some(o), .. }
                        if id == "call-1" && o == "ok\n")
            ));

            // Snapshot + delta interleave dedups by emitted length.
            let text_open = json!({"type": "message.part.updated", "properties": {"part": {
                "id": "prt_x", "messageID": "msg_a", "sessionID": "ses_child",
                "type": "text", "text": "",
            }}});
            assert!(handle_bus_event(&mut state, &text_open).is_empty());
            let delta = json!({"type": "message.part.delta", "properties": {
                "sessionID": "ses_child", "messageID": "msg_a", "partID": "prt_x",
                "field": "text", "delta": "finished ",
            }});
            let events = handle_bus_event(&mut state, &delta);
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::Subagent { event, .. }]
                    if matches!(&**event, AgentEvent::TextDelta { text } if text == "finished ")
            ));
            let settled = json!({"type": "message.part.updated", "properties": {"part": {
                "id": "prt_x", "messageID": "msg_a", "sessionID": "ses_child",
                "type": "text", "text": "finished ",
            }}});
            assert!(handle_bus_event(&mut state, &settled).is_empty());
        }
        // The ACP completion settles the chip: drain, then tagged Done (no
        // fallback text — the transcript already streamed).
        tracker.observe(&task_update(
            "t1",
            "completed",
            Some(json!({
                "output": "<task id=\"ses_child\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"sessionId": "ses_child"},
            })),
        ));
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("settles")
            .expect("event")
            .expect("ok");
        assert!(matches!(
            &ev,
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "t1"
                    && matches!(&**event, AgentEvent::Done { status: DoneStatus::Completed, .. })
        ));
    }

    #[tokio::test]
    async fn nested_children_and_foreign_sessions_never_bind() {
        let (mut tracker, _rx) = tracker();
        tracker.observe(&task_update("t1", "in_progress", None));
        let mut state = tracker.state.lock().unwrap();
        let nested = json!({"type": "session.created", "properties": {"info": {
            "id": "ses_grandchild", "parentID": "ses_child",
            "title": "Nested (@general subagent)",
        }}});
        handle_bus_event(&mut state, &nested);
        assert!(state.children.is_empty());
        assert_eq!(state.pending.len(), 1);
    }

    #[tokio::test]
    async fn teardown_settles_open_chips_interrupted() {
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = OpencodeTracker::new("ses_parent".into(), tx, None);
        {
            let mut state = tracker.state.lock().unwrap();
            state.children.insert(
                "ses_child".into(),
                ChildState {
                    parent_tool_use_id: "t1".into(),
                    saw_transcript: true,
                    ..ChildState::default()
                },
            );
        }
        drop(tracker);
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("settles")
            .expect("event")
            .expect("ok");
        assert!(matches!(
            &ev,
            AgentEvent::Subagent { parent_tool_use_id, event }
                if parent_tool_use_id == "t1"
                    && matches!(&**event, AgentEvent::Done { status: DoneStatus::Interrupted, .. })
        ));
    }

    #[test]
    fn tool_names_type_the_common_calls() {
        let call = oc_tool_call("bash", &json!({"command": "ls -la", "description": "list"}));
        assert_eq!(
            call,
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        let call = oc_tool_call("read", &json!({"filePath": "/w/a.rs"}));
        assert_eq!(
            call,
            ToolCall::ReadFile {
                path: "/w/a.rs".into()
            }
        );
        let call = oc_tool_call(
            "edit",
            &json!({"filePath": "/w/a.rs", "oldString": "a", "newString": "b"}),
        );
        assert_eq!(
            call,
            ToolCall::EditFile {
                path: "/w/a.rs".into(),
                old_string: Some("a".into()),
                new_string: Some("b".into()),
            }
        );
        let call = oc_tool_call("task", &json!({"description": "Scan crates"}));
        assert!(matches!(call, ToolCall::Unknown { name, .. } if name == "Agent: Scan crates"));
        let call = oc_tool_call("mystery", &json!({"x": 1}));
        assert!(matches!(call, ToolCall::Unknown { name, input: Some(_) } if name == "mystery"));
    }
}
