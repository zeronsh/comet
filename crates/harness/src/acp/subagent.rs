//! Grok subagent visualization on the ACP path.
//!
//! Grok's wire frames carry no `parent_tool_use_id` (the docs promise
//! tagging; unimplemented as of 1.0.4), and a subagent's interior transcript
//! never appears on the parent's ACP stream — the wire only carries the
//! lifecycle extension updates `subagent_spawned` / `subagent_progress` /
//! `subagent_finished` (verified live, 1.0.4). The transcript itself is
//! written incrementally to the child session's `chat_history.jsonl` under
//! `~/.grok/sessions/<urlencoded-cwd>/<child_session_id>/`.
//!
//! So the tracker correlates the `spawn_subagent` tool call with its
//! `subagent_spawned` update, tails the child's `chat_history.jsonl` into
//! tagged [`AgentEvent::Subagent`] events (the engine routes those to the
//! subagent's own doc and flips the spawn chip), and settles the chip with a
//! tagged `Done` from `subagent_finished`. The on-disk format is
//! vendor-private: every parse fails soft, degrading to chip + final output
//! (from the wire's `subagent_finished`) rather than erroring the run.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use zeron_proto::{AgentEvent, DoneStatus, ToolCall};

use crate::HarnessError;

use super::normalize::{cap_text, xai_tool_name, OUTPUT_CAP};

/// Cadence of the transcript tail (the CLI appends at message granularity, so
/// sub-second polling is plenty "live").
const TAIL_POLL: Duration = Duration::from_millis(250);
/// Post-`subagent_finished` drain: the wire signal can beat the CLI's last
/// disk flush, so keep reading briefly before settling the tagged Done.
const DRAIN_POLLS: u32 = 6;
const DRAIN_POLL: Duration = Duration::from_millis(200);

/// The default grok sessions root (`~/.grok/sessions`).
fn default_sessions_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".grok")
        .join("sessions")
}

/// Wrap an event as subagent-attributed traffic.
fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

fn done_status(status: &str) -> DoneStatus {
    match status {
        "failed" | "error" | "errored" => DoneStatus::Errored,
        "cancelled" | "canceled" | "killed" | "stopped" | "interrupted" => DoneStatus::Interrupted,
        _ => DoneStatus::Completed,
    }
}

/// A spawn chip (`spawn_subagent` tool call) not yet bound to a subagent id.
struct PendingSpawn {
    tool_call_id: String,
    description: String,
}

/// The `subagent_finished` payload handed to a live tail.
struct TailFinish {
    status: DoneStatus,
    output: String,
}

/// A `subagent_spawned` update that arrived before any chip could be bound
/// (defensive: not an ordering grok exhibits today).
struct SpawnedUnbound {
    child_session_id: String,
}

pub(crate) struct SubagentTracker {
    /// The parent ACP session — `subagent_spawned` for a NESTED spawn (a
    /// subagent's own subagent) must not bind to this feed's chips.
    session_id: String,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    sessions_root: PathBuf,
    /// Spawn chips seen on the wire, FIFO, awaiting a subagent id.
    pending: VecDeque<PendingSpawn>,
    /// subagent_id → the spawn chip's tool-use id.
    bound: HashMap<String, String>,
    /// `subagent_spawned` payloads that could not bind yet.
    spawned_unbound: HashMap<String, SpawnedUnbound>,
    /// subagent_id → finished-signal for the live tail task.
    tails: HashMap<String, oneshot::Sender<TailFinish>>,
}

impl SubagentTracker {
    pub(crate) fn new(
        session_id: String,
        event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
        sessions_root: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id,
            event_tx,
            sessions_root: sessions_root.unwrap_or_else(default_sessions_root),
            pending: VecDeque::new(),
            bound: HashMap::new(),
            spawned_unbound: HashMap::new(),
            tails: HashMap::new(),
        }
    }

    /// Inspect one `session/update` payload's `update` object. Pure
    /// bookkeeping — tagged events flow from the spawned tail tasks, which
    /// hold their own `event_tx` clones (they outlive the turn: a background
    /// subagent keeps streaming to its doc after the parent's Done, and the
    /// event stream stays open until every tail settles).
    pub(crate) fn observe(&mut self, update: &Value) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("tool_call") | Some("tool_call_update") => self.observe_tool_call(update),
            Some("subagent_spawned") => self.observe_spawned(update),
            Some("subagent_finished") => self.observe_finished(update),
            _ => {}
        }
    }

    fn observe_tool_call(&mut self, update: &Value) {
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.is_empty() {
            return;
        }
        if xai_tool_name(update) == Some("spawn_subagent") {
            let description = update
                .get("rawInput")
                .and_then(|r| r.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match self.pending.iter_mut().find(|p| p.tool_call_id == id) {
                Some(p) if p.description.is_empty() => p.description = description.to_owned(),
                Some(_) => {}
                None => self.pending.push_back(PendingSpawn {
                    tool_call_id: id.to_owned(),
                    description: description.to_owned(),
                }),
            }
        }
        // The spawn's completion echoes the minted id in its output text
        // ("subagent_id: <uuid>") — the completed update carries no _meta, so
        // key off membership in `pending` instead. Background spawns complete
        // here BEFORE `subagent_spawned` arrives; sync spawns after.
        if update.get("status").and_then(Value::as_str) == Some("completed")
            && self.pending.iter().any(|p| p.tool_call_id == id)
            && let Some(sub_id) = spawn_output_subagent_id(update)
        {
            self.pending.retain(|p| p.tool_call_id != id);
            self.bound.insert(sub_id.clone(), id.to_owned());
            if let Some(spawned) = self.spawned_unbound.remove(&sub_id) {
                self.start_tail(&sub_id, &spawned.child_session_id);
            }
        }
    }

    fn observe_spawned(&mut self, update: &Value) {
        // A nested spawn (a subagent spawning its own subagent) would carry
        // the CHILD's session id here — never bind it to this feed's chips.
        if let Some(parent) = update.get("parent_session_id").and_then(Value::as_str)
            && parent != self.session_id
        {
            return;
        }
        let Some(sub_id) = update
            .get("subagent_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let child_session_id = update
            .get("child_session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(sub_id)
            .to_owned();
        if !self.bound.contains_key(sub_id) {
            // Sync spawns: the chip hasn't completed yet — bind by
            // description (FIFO across identical descriptions, matching
            // grok's spawn order), else the oldest pending spawn.
            let description = update
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let ix = self
                .pending
                .iter()
                .position(|p| !description.is_empty() && p.description == description)
                .or(if self.pending.is_empty() { None } else { Some(0) });
            match ix.and_then(|i| self.pending.remove(i)) {
                Some(p) => {
                    self.bound.insert(sub_id.to_owned(), p.tool_call_id);
                }
                None => {
                    // No chip candidate on the wire (yet): stash — the
                    // spawn completion's id echo binds it later.
                    self.spawned_unbound.insert(
                        sub_id.to_owned(),
                        SpawnedUnbound { child_session_id },
                    );
                    return;
                }
            }
        }
        self.start_tail(sub_id, &child_session_id);
    }

    fn observe_finished(&mut self, update: &Value) {
        let Some(sub_id) = update.get("subagent_id").and_then(Value::as_str) else {
            return;
        };
        let finish = TailFinish {
            status: done_status(
                update
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed"),
            ),
            output: update
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        self.spawned_unbound.remove(sub_id);
        if let Some(tx) = self.tails.remove(sub_id) {
            let _ = tx.send(finish);
        } else if let Some(parent) = self.bound.get(sub_id).cloned() {
            // No tail ever started (the chip bound but `subagent_spawned`
            // never arrived, or the tail task failed to spawn): the wire's
            // final output still makes a transcript, and the chip settles.
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                if !finish.output.is_empty() {
                    let _ = event_tx
                        .send(Ok(tag(
                            &parent,
                            AgentEvent::TextDelta {
                                text: finish.output,
                            },
                        )))
                        .await;
                }
                let _ = event_tx
                    .send(Ok(tag(
                        &parent,
                        AgentEvent::Done {
                            status: finish.status,
                            result: None,
                            error: None,
                            session_id: None,
                        },
                    )))
                    .await;
            });
        }
    }

    fn start_tail(&mut self, sub_id: &str, child_session_id: &str) {
        let Some(parent) = self.bound.get(sub_id).cloned() else {
            return;
        };
        if self.tails.contains_key(sub_id) {
            return;
        }
        let (finished_tx, finished_rx) = oneshot::channel();
        self.tails.insert(sub_id.to_owned(), finished_tx);
        tokio::spawn(tail_task(
            self.event_tx.clone(),
            self.sessions_root.clone(),
            child_session_id.to_owned(),
            parent,
            finished_rx,
        ));
    }
}

/// `subagent_id: <id>` from the spawn tool call's completion output.
fn spawn_output_subagent_id(update: &Value) -> Option<String> {
    let text = update
        .get("rawOutput")
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .or_else(|| {
            update
                .get("content")?
                .as_array()?
                .iter()
                .find_map(|c| c.get("content")?.get("text")?.as_str())
        })?;
    text.lines().find_map(|line| {
        let id = line.trim().strip_prefix("subagent_id:")?.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

// ---------------------------------------------------------------------------
// Disk tail
// ---------------------------------------------------------------------------

/// The child session's `chat_history.jsonl`, located by session id ONE level
/// under the sessions root (`<root>/<urlencoded-cwd>/<child_session_id>/`) —
/// scanning dodges reimplementing grok's cwd encoding, and the child's cwd
/// can differ from the parent's anyway.
fn locate_history(root: &Path, child_session_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry
            .path()
            .join(child_session_id)
            .join("chat_history.jsonl");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Incremental line reader over an append-only JSONL file. A partial trailing
/// line (a write raced mid-append) carries over to the next poll.
struct TailReader {
    path: PathBuf,
    offset: u64,
    carry: Vec<u8>,
}

impl TailReader {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            carry: Vec::new(),
        }
    }

    /// New complete entries since the last poll; io/parse failures read as
    /// "nothing new" (fail soft — the format is vendor-private).
    fn read_new(&mut self) -> Vec<Value> {
        use std::io::{Read as _, Seek as _};
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if file.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        let Ok(read) = file.read_to_end(&mut bytes) else {
            return Vec::new();
        };
        self.offset += read as u64;
        self.carry.extend_from_slice(&bytes);
        let mut entries = Vec::new();
        while let Some(nl) = self.carry.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.carry.drain(..=nl).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                entries.push(v);
            }
        }
        entries
    }
}

/// Map one `chat_history.jsonl` entry to (untagged) transcript events.
/// Message-granularity by construction — grok only writes settled messages;
/// the trailing `\n\n` keeps consecutive message-level chunks readable when
/// the fold concatenates them.
fn entry_events(entry: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    match entry.get("type").and_then(Value::as_str) {
        Some("reasoning") => {
            let text: Vec<&str> = entry
                .get("summary")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|s| s.get("type").and_then(Value::as_str) == Some("summary_text"))
                .filter_map(|s| s.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect();
            if !text.is_empty() {
                events.push(AgentEvent::ReasoningDelta {
                    text: format!("{}\n\n", text.join("\n")),
                });
            }
        }
        Some("assistant") => {
            if let Some(text) = entry
                .get("content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                events.push(AgentEvent::TextDelta {
                    text: format!("{text}\n\n"),
                });
            }
            for tc in entry
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
            {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = tc.get("name").and_then(Value::as_str).unwrap_or_default();
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                // `arguments` is a JSON-encoded STRING on disk.
                let args = tc
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .unwrap_or(Value::Null);
                events.push(AgentEvent::ToolCall {
                    id: id.to_owned(),
                    call: grok_tool_call(name, &args),
                });
            }
        }
        Some("tool_result") => {
            if let Some(id) = entry
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|i| !i.is_empty())
            {
                let output = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(|t| cap_text(t, OUTPUT_CAP));
                events.push(AgentEvent::ToolResult {
                    id: id.to_owned(),
                    is_error: false,
                    output,
                    diff: None,
                });
            }
        }
        Some("user") => {
            // The message INTO the subagent — its spawn prompt (and any
            // future steer): its own user entry in the subagent doc, like
            // the chat it is. Synthetic context injections (reminder-style
            // tagged blocks) are not conversation.
            if let Some(text) = entry
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty() && !t.starts_with('<'))
            {
                events.push(AgentEvent::UserMessage {
                    text: text.to_owned(),
                });
            }
        }
        // system prompt / synthetic reminders: not transcript.
        _ => {}
    }
    events
}

/// Type a grok-native tool invocation (disk names, not ACP kinds).
fn grok_tool_call(name: &str, args: &Value) -> ToolCall {
    let s = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| args.get(*k))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    match name {
        "run_terminal_command" => ToolCall::Exec {
            command: s(&["command"]).unwrap_or_default(),
        },
        "read_file" => ToolCall::ReadFile {
            path: s(&["target_file", "file_path", "path"]).unwrap_or_default(),
        },
        "write_file" => ToolCall::WriteFile {
            path: s(&["target_file", "file_path", "path"]).unwrap_or_default(),
            content: s(&["contents", "content"]),
        },
        "search_replace" => ToolCall::EditFile {
            path: s(&["file_path", "target_file", "path"]).unwrap_or_default(),
            old_string: s(&["old_string"]),
            new_string: s(&["new_string"]),
        },
        "grep" => ToolCall::Search {
            pattern: s(&["pattern"]).unwrap_or_default(),
            path: s(&["path"]),
        },
        "glob" => ToolCall::Glob {
            pattern: s(&["pattern", "glob_pattern"]).unwrap_or_default(),
        },
        "web_search" => ToolCall::WebSearch {
            query: s(&["query", "search_term"]).unwrap_or_default(),
        },
        // A nested spawn inside the subagent: same naming as the parent chip
        // (no recursive tail — it renders as a plain chip in the child doc).
        "spawn_subagent" => ToolCall::Unknown {
            name: s(&["description"])
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: (!args.is_null()).then(|| args.clone()),
        },
        _ => ToolCall::Unknown {
            name: name.to_owned(),
            input: (!args.is_null()).then(|| args.clone()),
        },
    }
}

/// One subagent's transcript tail: poll the child's `chat_history.jsonl`
/// into tagged events until `subagent_finished` (or tracker teardown), then
/// drain briefly and settle the chip with a tagged Done. Holds its own
/// `event_tx` clone, so the run's event stream stays open until the tail
/// settles — an agent that dies mid-subagent still gets its chip closed.
async fn tail_task(
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    root: PathBuf,
    child_session_id: String,
    parent_tool_use_id: String,
    mut finished_rx: oneshot::Receiver<TailFinish>,
) {
    let mut reader: Option<TailReader> = None;
    let mut emitted = false;
    let pump = |reader: &mut Option<TailReader>| -> Vec<AgentEvent> {
        if reader.is_none() {
            *reader = locate_history(&root, &child_session_id).map(TailReader::new);
        }
        reader
            .as_mut()
            .map(|r| r.read_new())
            .unwrap_or_default()
            .iter()
            .flat_map(entry_events)
            .collect()
    };

    let finish = loop {
        for ev in pump(&mut reader) {
            emitted = true;
            if event_tx
                .send(Ok(tag(&parent_tool_use_id, ev)))
                .await
                .is_err()
            {
                return;
            }
        }
        if event_tx.is_closed() {
            return;
        }
        tokio::select! {
            fin = &mut finished_rx => {
                // A dropped sender is session teardown with the subagent
                // still running: settle the chip as interrupted rather than
                // leaving it spinning forever.
                break fin.unwrap_or(TailFinish {
                    status: DoneStatus::Interrupted,
                    output: String::new(),
                });
            }
            _ = tokio::time::sleep(TAIL_POLL) => {}
        }
    };

    // The finished signal can beat the CLI's last disk flush: drain until a
    // quiet poll (or the budget), then settle.
    for i in 0..DRAIN_POLLS {
        let events = pump(&mut reader);
        for ev in events.iter().cloned() {
            emitted = true;
            if event_tx
                .send(Ok(tag(&parent_tool_use_id, ev)))
                .await
                .is_err()
            {
                return;
            }
        }
        if events.is_empty() && i > 0 {
            break;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
    if !emitted && !finish.output.is_empty() {
        // The tail never found (or never parsed) the transcript: the wire's
        // final output alone still makes a useful doc.
        if event_tx
            .send(Ok(tag(
                &parent_tool_use_id,
                AgentEvent::TextDelta {
                    text: finish.output,
                },
            )))
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = event_tx
        .send(Ok(tag(
            &parent_tool_use_id,
            AgentEvent::Done {
                status: finish.status,
                result: None,
                error: None,
                session_id: None,
            },
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spawn_completion_output_yields_the_subagent_id() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "rawOutput": {"type": "Text", "text": "Subagent started in background.\nsubagent_id: 01a0-abc\ntype: explore"},
        });
        assert_eq!(
            spawn_output_subagent_id(&update).as_deref(),
            Some("01a0-abc")
        );
        // content-block fallback when rawOutput is absent.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "subagent_id: xyz"}}],
        });
        assert_eq!(spawn_output_subagent_id(&update).as_deref(), Some("xyz"));
    }

    #[test]
    fn chat_history_entries_map_to_transcript_events() {
        // Real shapes from ~/.grok/sessions (grok 1.0.4).
        let reasoning = json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "The user wants a probe.\n"}],
            "encrypted_content": "opaque", "status": "completed",
        });
        assert!(matches!(
            entry_events(&reasoning).as_slice(),
            [AgentEvent::ReasoningDelta { text }] if text.starts_with("The user wants a probe.")
        ));

        let assistant = json!({
            "type": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call-1-0",
                "name": "read_file",
                "arguments": "{\"target_file\":\"/w/README.md\",\"limit\":80}",
            }],
            "model_id": "grok-4.6-build",
        });
        assert!(matches!(
            entry_events(&assistant).as_slice(),
            [AgentEvent::ToolCall { id, call: ToolCall::ReadFile { path } }]
                if id == "call-1-0" && path == "/w/README.md"
        ));

        let result = json!({
            "type": "tool_result",
            "tool_call_id": "call-1-0",
            "content": "1→# zeron — Architecture",
        });
        assert!(matches!(
            entry_events(&result).as_slice(),
            [AgentEvent::ToolResult { id, is_error: false, output: Some(o), .. }]
                if id == "call-1-0" && o.contains("Architecture")
        ));

        let closing = json!({"type": "assistant", "content": "finished", "model_id": "m"});
        assert!(matches!(
            entry_events(&closing).as_slice(),
            [AgentEvent::TextDelta { text }] if text == "finished\n\n"
        ));

        // The message INTO the subagent is its own user entry; system
        // entries and reminder-style synthetic injections are skipped.
        assert!(matches!(
            entry_events(&json!({"type": "user", "content": "scan the fold path"})).as_slice(),
            [AgentEvent::UserMessage { text }] if text == "scan the fold path"
        ));
        assert!(entry_events(&json!({"type": "system", "content": "x"})).is_empty());
        assert!(
            entry_events(&json!({"type": "user", "content": "<system-reminder>tick</system-reminder>"}))
                .is_empty()
        );
    }

    #[test]
    fn grok_tool_names_type_the_common_calls() {
        let call = grok_tool_call("run_terminal_command", &json!({"command": "ls -la"}));
        assert_eq!(call, ToolCall::Exec { command: "ls -la".into() });
        let call = grok_tool_call("grep", &json!({"pattern": "0\\.1", "glob": "*.rs"}));
        assert_eq!(call, ToolCall::Search { pattern: "0\\.1".into(), path: None });
        let call = grok_tool_call(
            "search_replace",
            &json!({"file_path": "/w/a.rs", "old_string": "a", "new_string": "b"}),
        );
        assert_eq!(
            call,
            ToolCall::EditFile {
                path: "/w/a.rs".into(),
                old_string: Some("a".into()),
                new_string: Some("b".into()),
            }
        );
        let call = grok_tool_call("spawn_subagent", &json!({"description": "Scan crates"}));
        assert!(matches!(call, ToolCall::Unknown { name, .. } if name == "Agent: Scan crates"));
        let call = grok_tool_call("mystery_tool", &json!({"x": 1}));
        assert!(matches!(call, ToolCall::Unknown { name, input: Some(_) } if name == "mystery_tool"));
    }
}
