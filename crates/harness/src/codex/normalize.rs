//! Codex app-server notification/item → [`AgentEvent`] mapping, ported from
//! codex.ts's `mapItem`/notification switch.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase/snake_case item
//! types) are accepted, and unknown item types map to nothing.

use serde_json::Value;
use zeron_proto::{AgentEvent, DoneStatus, TodoItem, ToolCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Completed,
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    field(v, keys)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Delta text under either spelling the app server has used
/// (`delta` on agentMessage, `textDelta` on some reasoning builds).
pub(crate) fn delta_text(params: &Value) -> Option<String> {
    field(params, &["delta", "textDelta"])
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Codex streams independent reasoning items and indexed parts without text
/// separators. Preserve those boundaries before the document folds deltas
/// together; otherwise adjacent Markdown headings become `**one****two**`.
/// One instance belongs to one thread, so child activity cannot split a
/// parent's in-flight paragraph.
#[derive(Default)]
pub(crate) struct ReasoningStream {
    last_part: Option<(String, bool, u64)>,
    announced_summary: Option<(String, u64)>,
    trailing_newlines: usize,
}

impl ReasoningStream {
    pub(crate) fn map(&mut self, method: &str, params: &Value) -> Vec<AgentEvent> {
        let id = item_id(params);
        let summary = method != "item/reasoning/textDelta";
        let index = field(
            params,
            if summary {
                &["summaryIndex", "summary_index"]
            } else {
                &["contentIndex", "content_index"]
            },
        )
        .and_then(Value::as_u64)
        .or_else(|| {
            self.announced_summary
                .as_ref()
                .filter(|(item, _)| summary && item == &id)
                .map(|(_, index)| *index)
        })
        .unwrap_or(0);
        if method == "item/reasoning/summaryPartAdded" {
            self.announced_summary = Some((id, index));
            return Vec::new();
        }
        let Some(text) = delta_text(params) else {
            return Vec::new();
        };
        let part = (id, summary, index);
        let mut events = Vec::new();
        if self.last_part.as_ref().is_some_and(|last| last != &part) {
            let leading_newlines = text.chars().take_while(|&c| c == '\n').count();
            let missing = 2usize.saturating_sub(self.trailing_newlines + leading_newlines);
            if missing > 0 {
                events.push(AgentEvent::ReasoningDelta {
                    text: "\n".repeat(missing),
                });
                self.trailing_newlines += missing;
            }
        }
        let trailing = text.chars().rev().take_while(|&c| c == '\n').count();
        self.trailing_newlines = if trailing == text.len() {
            self.trailing_newlines + trailing
        } else {
            trailing
        };
        self.last_part = Some(part);
        events.push(AgentEvent::ReasoningDelta { text });
        events
    }
}

pub(crate) fn item_id(params: &Value) -> String {
    str_field(params, &["itemId", "item_id"])
}

/// `params.turn.id` on the turn/* lifecycle notifications.
pub(crate) fn turn_id(params: &Value) -> String {
    params
        .get("turn")
        .map(|t| str_field(t, &["id"]))
        .unwrap_or_default()
}

/// `params.turn.error.message` (turn/completed carries an optional error;
/// turn/failed always should).
pub(crate) fn turn_error_message(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("error"))
        .filter(|e| !e.is_null())
        .map(|e| {
            let msg = str_field(e, &["message"]);
            if msg.is_empty() { e.to_string() } else { msg }
        })
}

/// `thread/tokenUsage/updated` → a [`AgentEvent::Usage`] snapshot of the LAST
/// turn's tokens (held by the session loop, emitted before `Done`).
pub(crate) fn usage_event(params: &Value) -> Option<AgentEvent> {
    let last = field(params, &["tokenUsage", "token_usage"])?.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Some(AgentEvent::Usage {
        input_tokens: count(&["inputTokens", "input_tokens"]),
        output_tokens: count(&["outputTokens", "output_tokens"]),
    })
}

pub(crate) fn context_usage_event(params: &Value) -> Option<AgentEvent> {
    let usage = field(params, &["tokenUsage", "token_usage"])?;
    let last = usage.get("last");
    let tokens = last
        .and_then(|last| field(last, &["totalTokens", "total_tokens"]).and_then(Value::as_u64))
        .or_else(|| {
            let last = last?;
            let input = field(last, &["inputTokens", "input_tokens"])?.as_u64()?;
            Some(
                input.saturating_add(
                    field(last, &["outputTokens", "output_tokens"])
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                ),
            )
        });
    let window = field(usage, &["modelContextWindow", "model_context_window"])
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    (tokens.is_some() || window.is_some()).then_some(AgentEvent::ContextUsage { tokens, window })
}

/// Tool-shaped Codex items must always close the lifecycle they open: started
/// opens the ToolCall, completed refreshes its metadata and resolves the same
/// stable id (port of codex.ts `toolLifecycle`).
fn tool_lifecycle(phase: Phase, id: String, call: ToolCall, is_error: bool) -> Vec<AgentEvent> {
    match phase {
        Phase::Started => vec![AgentEvent::ToolCall { id, call }],
        Phase::Completed => vec![
            AgentEvent::ToolCall {
                id: id.clone(),
                call,
            },
            AgentEvent::ToolResult {
                id,
                is_error,
                output: None,
                diff: None,
            },
        ],
    }
}

/// A `fileChange` item's `changes` array reduced to the typed [`ToolCall`] the
/// UI renders: a lone `add` is a file write, a lone `update` an edit, anything
/// else (deletes, multi-file changes) a patch.
fn file_change_call(changes: &[(String, String)]) -> ToolCall {
    match changes {
        [(path, kind)] if kind == "add" => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        [(path, kind)] if kind == "update" => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        [(path, _)] => ToolCall::ApplyPatch {
            path: Some(path.clone()),
        },
        _ => ToolCall::ApplyPatch { path: None },
    }
}

pub(crate) fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

pub(super) fn is_collab_spawn(item: &Value) -> bool {
    matches!(
        item_type(item),
        "collabAgentToolCall" | "collab_agent_tool_call"
    ) && matches!(
        item.get("tool").and_then(Value::as_str),
        Some("spawnAgent" | "spawn_agent")
    )
}

/// A v1 spawn names its child in the completed result. Other collaboration
/// tools can address several receivers, but only a spawn owns a transcript.
pub(super) fn collab_spawn_child(item: &Value) -> Option<&str> {
    if !is_collab_spawn(item)
        || matches!(
            item.get("status").and_then(Value::as_str),
            Some("failed" | "errored")
        )
    {
        return None;
    }
    let receivers = field(item, &["receiverThreadIds", "receiver_thread_ids"])?.as_array()?;
    match receivers.as_slice() {
        [child] => child.as_str().filter(|id| !id.is_empty()),
        _ => None,
    }
}

/// Map one `item/started` or `item/completed` payload's item to events.
/// `agentMessage` and `reasoning` flow through their delta channels and are
/// handled by the session loop, not here.
pub(crate) fn map_item(phase: Phase, item: &Value) -> Vec<AgentEvent> {
    let id = str_field(item, &["id"]);
    let status = str_field(item, &["status"]);
    match item_type(item) {
        "commandExecution" | "command_execution" => match phase {
            Phase::Started => vec![AgentEvent::ToolCall {
                id,
                call: ToolCall::Exec {
                    command: str_field(item, &["command"]),
                },
            }],
            Phase::Completed => {
                let exit_code = field(item, &["exitCode", "exit_code"])
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                vec![AgentEvent::ToolResult {
                    id,
                    is_error: status == "failed" || exit_code != 0,
                    output: None,
                    diff: None,
                }]
            }
        },
        "fileChange" | "file_change" => {
            let changes: Vec<(String, String)> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    // Unknown kinds degrade to "update", like codex.ts.
                    let kind = c
                        .get("kind")
                        .and_then(Value::as_str)
                        .filter(|k| matches!(*k, "add" | "delete" | "update"))
                        .unwrap_or("update");
                    (str_field(c, &["path"]), kind.to_owned())
                })
                .collect();
            tool_lifecycle(
                phase,
                id,
                file_change_call(&changes),
                status == "failed" || status == "declined",
            )
        }
        "mcpToolCall" | "mcp_tool_call" => match phase {
            Phase::Started => {
                let input = item.get("arguments").filter(|v| !v.is_null()).cloned();
                vec![AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Mcp {
                        server: str_field(item, &["server"]),
                        tool: str_field(item, &["tool"]),
                        input,
                    },
                }]
            }
            Phase::Completed => vec![AgentEvent::ToolResult {
                id,
                is_error: status == "failed",
                output: None,
                diff: None,
            }],
        },
        "webSearch" | "web_search" => tool_lifecycle(
            phase,
            id,
            ToolCall::WebSearch {
                query: str_field(item, &["query"]),
            },
            false,
        ),
        "todoList" | "todo_list" => {
            let items = item
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, &["text"]),
                    done: field(t, &["completed", "done"]).and_then(Value::as_bool) == Some(true),
                })
                .collect();
            tool_lifecycle(phase, id, ToolCall::Todo { items }, false)
        }
        "error" => vec![AgentEvent::Error {
            message: str_field(item, &["message"]),
        }],
        "collabAgentToolCall" | "collab_agent_tool_call" => {
            let tool = str_field(item, &["tool"]);
            let name = if is_collab_spawn(item) {
                "Agent".to_owned()
            } else {
                match tool.as_str() {
                    "sendInput" | "send_input" => "Send agent message".to_owned(),
                    "wait" => "Wait for agents".to_owned(),
                    "closeAgent" | "close_agent" => "Close agent".to_owned(),
                    "resumeAgent" | "resume_agent" => "Resume agent".to_owned(),
                    _ => format!("Agent control: {tool}"),
                }
            };
            tool_lifecycle(
                phase,
                id,
                ToolCall::Unknown {
                    name,
                    input: Some(item.clone()),
                },
                matches!(status.as_str(), "failed" | "errored"),
            )
        }
        // A subagent spawn/lifecycle marker on the PARENT thread (multi-agent
        // v2, codex 0.146.x): the parent-feed chip for the child thread. The
        // child's own traffic routes separately (see `route_child_notification`
        // in mod.rs); this is only the spawn tool call the chip folds from.
        "subAgentActivity" | "sub_agent_activity" => {
            // A lifecycle marker has its own item id. It is not another
            // spawn; the child's turn notifications carry its terminal state.
            if !matches!(
                item.get("kind").and_then(Value::as_str),
                Some("started" | "spawned")
            ) {
                return Vec::new();
            }
            let name = str_field(item, &["agentPath"])
                .rsplit('/')
                .find(|s| !s.is_empty())
                .map(|leaf| format!("Agent: {leaf}"))
                .unwrap_or_else(|| "Agent".to_owned());
            tool_lifecycle(
                phase,
                id,
                ToolCall::Unknown {
                    name,
                    input: Some(item.clone()),
                },
                matches!(str_field(item, &["kind"]).as_str(), "failed" | "errored"),
            )
        }
        // reasoning / agentMessage flow through delta channels; the PARENT
        // feed's userMessage items are echoes of prompts we sent (already in
        // the doc). A CHILD thread's userMessage is different — the parent
        // steering its subagent — and is mapped where child items route
        // (mod.rs child branch), not here.
        _ => Vec::new(),
    }
}

/// The text of a `userMessage` thread item. Codex builds have carried both
/// shapes: a plain `text` field and a `content` array of text blocks.
pub(crate) fn user_message_text(item: &Value) -> Option<String> {
    let text = str_field(item, &["text"]);
    if !text.trim().is_empty() {
        return Some(text);
    }
    let joined: String = item
        .get("content")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Per-child stream state, retained across parent turns and follow-up tasks.
/// Completion-only messages need the same text fallback as the root. Replayed
/// user items must not reopen a finished document or duplicate a steering entry.
#[derive(Default)]
pub(super) struct ChildStream {
    reasoning: ReasoningStream,
    streamed_text: std::collections::HashSet<String>,
    completed_items: std::collections::VecDeque<String>,
    completed_turns: std::collections::VecDeque<String>,
    settled: bool,
}

fn remember(ids: &mut std::collections::VecDeque<String>, id: String) -> bool {
    if id.is_empty() {
        return true;
    }
    if ids.contains(&id) {
        return false;
    }
    if ids.len() == 256 {
        ids.pop_front();
    }
    ids.push_back(id);
    true
}

impl ChildStream {
    pub(super) fn map(&mut self, child: &str, method: &str, params: &Value) -> Vec<AgentEvent> {
        if method == "turn/started" {
            let id = turn_id(params);
            if !id.is_empty() && self.completed_turns.contains(&id) {
                return Vec::new();
            }
            if self.settled {
                self.settled = false;
                // v2 followup_task starts a new child turn without echoing a
                // userMessage. Reopen the existing document without inventing
                // a user prompt or attributing an activity id as a new spawn.
                return vec![AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: None,
                }];
            }
            return Vec::new();
        }
        if matches!(method, "item/started" | "item/completed") {
            let item = params.get("item").unwrap_or(&Value::Null);
            let phase = if method == "item/started" {
                Phase::Started
            } else {
                Phase::Completed
            };
            if phase == Phase::Completed
                && !remember(&mut self.completed_items, str_field(item, &["id"]))
            {
                return Vec::new();
            }
            if matches!(item_type(item), "userMessage" | "user_message") {
                return if phase == Phase::Completed {
                    user_message_text(item)
                        .map(|text| {
                            self.settled = false;
                            AgentEvent::UserMessage { text }
                        })
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                };
            }
            if self.settled {
                return Vec::new();
            }
            if matches!(item_type(item), "agentMessage" | "agent_message") {
                if phase == Phase::Started {
                    return Vec::new();
                }
                let mut events = Vec::new();
                if !self.streamed_text.remove(&str_field(item, &["id"])) {
                    let text = str_field(item, &["text"]);
                    if !text.is_empty() {
                        events.push(AgentEvent::TextDelta { text });
                    }
                }
                events.push(AgentEvent::TextDelta {
                    text: "\n\n".into(),
                });
                return events;
            }
            return map_item(phase, item);
        }
        if self.settled {
            return Vec::new();
        }
        match method {
            "item/agentMessage/delta" => {
                let id = item_id(params);
                if !id.is_empty() && self.completed_items.contains(&id) {
                    return Vec::new();
                }
                self.streamed_text.insert(id);
                delta_text(params)
                    .map(|text| AgentEvent::TextDelta { text })
                    .into_iter()
                    .collect()
            }
            "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded" => self.reasoning.map(method, params),
            "turn/completed" | "turn/failed" | "turn/aborted" | "thread/closed" => {
                if method != "thread/closed"
                    && !remember(&mut self.completed_turns, turn_id(params))
                {
                    return Vec::new();
                }
                self.settled = true;
                self.streamed_text.clear();
                let error = turn_error_message(params);
                let status = if method == "turn/failed"
                    || error.is_some()
                    || params.pointer("/turn/status").and_then(Value::as_str) == Some("failed")
                {
                    DoneStatus::Errored
                } else if method == "turn/aborted"
                    || params.pointer("/turn/status").and_then(Value::as_str) == Some("interrupted")
                {
                    DoneStatus::Interrupted
                } else {
                    DoneStatus::Completed
                };
                vec![AgentEvent::Done {
                    status,
                    result: None,
                    error,
                    session_id: Some(child.to_owned()),
                }]
            }
            "error" => vec![AgentEvent::Error {
                message: params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| params.get("message").and_then(Value::as_str))
                    .unwrap_or("Codex subagent error")
                    .to_owned(),
            }],
            _ => Vec::new(),
        }
    }
}

/// The thread a notification is addressed to: `thread/started` carries it at
/// `params.thread.id`, everything else at `params.threadId`. `None` for
/// thread-less methods (old builds, account noise).
pub(crate) fn notification_thread_id(method: &str, params: &Value) -> Option<String> {
    if method == "thread/started" {
        return params
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    field(params, &["threadId", "thread_id"])
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// How a notification addressed to a REGISTERED child thread is handled.
///
/// Exported and pure so the table can be asserted directly. The shape (and
/// the fail-open default) is load-bearing: two shipped bugs in t3code's codex
/// runtime came from a catch-all that swallowed everything a child emitted —
/// a child's `error` vanished (the agent card stayed running forever) and a
/// swallowed `serverRequest/resolved` left the parent's approvals stuck.
///
/// - `Subagent`: content/lifecycle attributed to the child (item lifecycles,
///   errors, closure) — mapped to tagged [`AgentEvent::Subagent`] events.
/// - `Consumed`: child bookkeeping with no parent or subagent-doc meaning,
///   plus child thread-lifecycle methods that would rewrite PARENT state if
///   let through (`thread/started` repeats, status/name/usage updates).
/// - `Parent`: pass through to the parent path — unknown methods land here BY
///   DESIGN, so a codex update that adds a notification degrades to "the
///   parent sees it", never to silent loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildRoute {
    Subagent,
    Consumed,
    Parent,
}

pub(crate) fn route_child_notification(method: &str) -> ChildRoute {
    match method {
        // Child message/reasoning deltas DO stream on this wire tagged with
        // the child's threadId (live-verified against codex-cli 0.146.1
        // multi-agent capture) — the subagent transcript is token-level live
        // without touching the rollout file. Child TURN ends are the
        // subagent's terminal signal: `thread/closed` fires only via the
        // collab close_agent tool, which real fan-outs never call
        // (live-verified — chips stayed "running" forever without this).
        "item/started"
        | "item/completed"
        | "item/agentMessage/delta"
        | "item/reasoning/textDelta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/summaryPartAdded"
        | "turn/started"
        | "turn/completed"
        | "turn/failed"
        | "turn/aborted"
        | "error"
        | "thread/closed" => ChildRoute::Subagent,
        // Child status bookkeeping with no subagent meaning: consumed
        // so it can never settle the PARENT turn (the exact bug class the
        // explicit table exists for).
        "thread/status/changed"
        | "thread/tokenUsage/updated"
        // Child chatter with no consumer on this wire.
        | "item/commandExecution/outputDelta"
        | "item/fileChange/outputDelta"
        | "item/fileChange/patchUpdated"
        | "item/plan/delta"
        | "turn/plan/updated"
        | "turn/diff/updated"
        | "thread/name/updated"
        | "thread/settings/updated"
        | "rawResponseItem/completed"
        // Child-owned thread lifecycle that maps onto PARENT state in a
        // naive passthrough (archived/compacted), plus repeat thread/started.
        | "thread/archived"
        | "thread/unarchived"
        | "thread/compacted"
        | "thread/started" => ChildRoute::Consumed,
        // Unknown or parent-owned (approvals bookkeeping, account noise).
        _ => ChildRoute::Parent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_parts_preserve_chunking_and_existing_paragraph_breaks() {
        let mut stream = ReasoningStream::default();
        let mut text = String::new();
        for params in [
            json!({"itemId":"r1", "contentIndex":0, "textDelta":"**First"}),
            json!({"itemId":"r1", "contentIndex":0, "textDelta":" heading**\n"}),
            json!({"itemId":"r1", "contentIndex":1, "delta":"\nSecond paragraph.\n\n"}),
            // An empty delta must not consume the next item's boundary.
            json!({"itemId":"r2", "contentIndex":0, "delta":""}),
            json!({"item_id":"r2", "content_index":0, "delta":"Third paragraph."}),
        ] {
            for event in stream.map("item/reasoning/textDelta", &params) {
                if let AgentEvent::ReasoningDelta { text: delta } = event {
                    text.push_str(&delta);
                }
            }
        }
        assert_eq!(
            text,
            "**First heading**\n\nSecond paragraph.\n\nThird paragraph."
        );
    }

    #[test]
    fn user_message_text_accepts_both_shapes() {
        assert_eq!(
            user_message_text(&json!({"text": "steer"})),
            Some("steer".into())
        );
        assert_eq!(
            user_message_text(&json!({"content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ]})),
            Some("a\n\nb".into())
        );
        assert_eq!(user_message_text(&json!({"text": "  "})), None);
        assert_eq!(user_message_text(&json!({})), None);
    }

    #[test]
    fn delta_accepts_both_spellings() {
        assert_eq!(delta_text(&json!({"delta": "a"})), Some("a".into()));
        assert_eq!(delta_text(&json!({"textDelta": "b"})), Some("b".into()));
        assert_eq!(delta_text(&json!({"delta": ""})), None);
        assert_eq!(delta_text(&json!({})), None);
    }

    #[test]
    fn command_execution_maps_exit_code_to_error() {
        let started = map_item(
            Phase::Started,
            &json!({"type": "commandExecution", "id": "c1", "command": "ls"}),
        );
        assert_eq!(
            started,
            vec![AgentEvent::ToolCall {
                id: "c1".into(),
                call: ToolCall::Exec {
                    command: "ls".into()
                },
            }]
        );
        let completed = map_item(
            Phase::Completed,
            &json!({"type": "command_execution", "id": "c1", "status": "completed", "exit_code": 2}),
        );
        assert_eq!(
            completed,
            vec![AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true,
                output: None,
                diff: None,
            }]
        );
    }

    #[test]
    fn file_change_variants_map_to_typed_calls() {
        let add = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f1", "changes": [{"path": "/a.rs", "kind": "add"}]}),
        );
        assert_eq!(
            add,
            vec![AgentEvent::ToolCall {
                id: "f1".into(),
                call: ToolCall::WriteFile {
                    path: "/a.rs".into(),
                    content: None
                },
            }]
        );
        let update = map_item(
            Phase::Completed,
            &json!({"type": "fileChange", "id": "f2", "status": "declined",
                    "changes": [{"path": "/b.rs", "kind": "update"}]}),
        );
        assert_eq!(
            update,
            vec![
                AgentEvent::ToolCall {
                    id: "f2".into(),
                    call: ToolCall::EditFile {
                        path: "/b.rs".into(),
                        old_string: None,
                        new_string: None
                    },
                },
                AgentEvent::ToolResult {
                    id: "f2".into(),
                    is_error: true,
                    output: None,
                    diff: None,
                },
            ]
        );
        let multi = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f3",
                    "changes": [{"path": "/a"}, {"path": "/b", "kind": "delete"}]}),
        );
        assert_eq!(
            multi,
            vec![AgentEvent::ToolCall {
                id: "f3".into(),
                call: ToolCall::ApplyPatch { path: None },
            }]
        );
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}}})),
            Some(AgentEvent::Usage {
                input_tokens: 42,
                output_tokens: 7
            })
        );
        assert_eq!(
            usage_event(&json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}}})),
            Some(AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2
            })
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    #[test]
    fn v1_spawns_and_controls_have_distinct_roles() {
        for tool in ["spawnAgent", "spawn_agent"] {
            let mut item = json!({"type":"collabAgentToolCall", "id":"spawn", "tool":tool,
                "status":"completed", "receiverThreadIds":["child"], "model":"child-model"});
            assert_eq!(collab_spawn_child(&item), Some("child"));
            let events = map_item(Phase::Completed, &item);
            assert!(matches!(&events[0], AgentEvent::ToolCall { call, .. }
                if call.is_subagent_spawn() && call.subagent_model() == Some("child-model")));
            assert!(matches!(
                &events[1],
                AgentEvent::ToolResult {
                    is_error: false,
                    ..
                }
            ));
            item["status"] = "failed".into();
            assert_eq!(collab_spawn_child(&item), None);
            assert!(matches!(
                map_item(Phase::Completed, &item).last(),
                Some(AgentEvent::ToolResult { is_error: true, .. })
            ));
        }
        for tool in [
            "sendInput",
            "send_input",
            "wait",
            "closeAgent",
            "resumeAgent",
            "futureControl",
        ] {
            let item = json!({"type":"collabAgentToolCall", "id":"control", "tool":tool,
                "receiverThreadIds":["child"]});
            assert_eq!(collab_spawn_child(&item), None);
            assert!(
                matches!(&map_item(Phase::Started, &item)[0], AgentEvent::ToolCall { call, .. } if !call.is_subagent_spawn())
            );
        }
        for receivers in [json!([]), json!(["one", "two"]), json!([""])] {
            assert_eq!(
                collab_spawn_child(
                    &json!({"type":"collabAgentToolCall", "tool":"spawnAgent", "receiverThreadIds":receivers})
                ),
                None
            );
        }
    }

    #[test]
    fn activity_updates_are_not_spawns_in_either_item_phase() {
        for kind in [
            "interacted",
            "completed",
            "failed",
            "errored",
            "futureActivity",
        ] {
            let item = json!({"type":"subAgentActivity", "id":"activity", "kind":kind,
                "agentThreadId":"child", "agentPath":"/root/alpha"});
            assert!(map_item(Phase::Started, &item).is_empty());
            assert!(map_item(Phase::Completed, &item).is_empty());
        }
    }

    #[test]
    fn sub_agent_activity_maps_to_a_named_parent_chip() {
        let started = map_item(
            Phase::Started,
            &json!({"type": "subAgentActivity", "id": "call_1", "kind": "started",
                    "agentThreadId": "child-1", "agentPath": "/root/alpha"}),
        );
        assert_eq!(started.len(), 1);
        assert!(matches!(
            &started[0],
            AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
                if id == "call_1" && name == "Agent: alpha"
        ));
        let completed = map_item(
            Phase::Completed,
            &json!({"type": "subAgentActivity", "id": "call_1", "kind": "started",
                    "agentThreadId": "child-1", "agentPath": "/root/alpha"}),
        );
        assert!(matches!(
            completed.last(),
            Some(AgentEvent::ToolResult { id, is_error: false, .. }) if id == "call_1"
        ));
    }

    #[test]
    fn child_routing_table_fails_open_to_parent() {
        // Child content/lifecycle → the subagent path, deltas included
        // (live-verified: child threads stream them on this wire).
        for m in [
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/reasoning/textDelta",
            "error",
            "thread/closed",
        ] {
            assert_eq!(route_child_notification(m), ChildRoute::Subagent, "{m}");
        }
        // Child TURN ENDS are the subagent's terminal signal…
        for m in ["turn/completed", "turn/aborted", "turn/failed"] {
            assert_eq!(route_child_notification(m), ChildRoute::Subagent, "{m}");
        }
        // A child turn start can reopen a completed assignment. It must never
        // reach the parent turn router.
        assert_eq!(
            route_child_notification("turn/started"),
            ChildRoute::Subagent
        );
        // Child-owned thread lifecycle would rewrite parent state — consumed.
        for m in ["thread/archived", "thread/compacted", "thread/started"] {
            assert_eq!(route_child_notification(m), ChildRoute::Consumed, "{m}");
        }
        // Unknown methods degrade to "parent sees it", never silent loss
        // (the two-shipped-bugs rule).
        for m in [
            "serverRequest/resolved",
            "thread/somethingBrandNew",
            "account/rateLimits/updated",
        ] {
            assert_eq!(route_child_notification(m), ChildRoute::Parent, "{m}");
        }
    }

    #[test]
    fn notification_thread_ids_read_both_shapes() {
        assert_eq!(
            notification_thread_id("thread/started", &json!({"thread": {"id": "th-c"}})),
            Some("th-c".into())
        );
        assert_eq!(
            notification_thread_id(
                "turn/completed",
                &json!({"threadId": "th-1", "turn": {"id": "t"}})
            ),
            Some("th-1".into())
        );
        assert_eq!(
            notification_thread_id("error", &json!({"message": "x"})),
            None
        );
    }

    #[test]
    fn turn_error_extraction() {
        assert_eq!(
            turn_error_message(&json!({"turn": {"id": "t", "error": {"message": "boom"}}})),
            Some("boom".into())
        );
        assert_eq!(turn_error_message(&json!({"turn": {"id": "t"}})), None);
        assert_eq!(turn_error_message(&json!({"turn": {"error": null}})), None);
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn context_uses_latest_call_not_thread_total() {
        assert_eq!(
            context_usage_event(&json!({"tokenUsage": {
                "last": {"totalTokens": 42000}, "total": {"totalTokens": 900000}, "modelContextWindow": 200000
            }})),
            Some(AgentEvent::ContextUsage {
                tokens: Some(42000),
                window: Some(200000)
            })
        );
        assert_eq!(
            context_usage_event(&json!({"token_usage": {
                "last": {"input_tokens": 0, "output_tokens": 0}, "model_context_window": 0
            }})),
            Some(AgentEvent::ContextUsage {
                tokens: Some(0),
                window: None
            })
        );
        assert_eq!(
            context_usage_event(&json!({"tokenUsage": {"total": {"totalTokens": 100}}})),
            None
        );
    }
}
