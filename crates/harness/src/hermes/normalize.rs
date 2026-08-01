//! ACP `session/update` → [`AgentEvent`] mapping for Hermes.
//!
//! Hermes renders its own tool calls before they reach the wire
//! (`acp_adapter/tools.py`): every ACP `tool_call` carries a `kind`, a
//! human-readable `title`, `locations`, and — for tools NOT in its "polished"
//! set — a `rawInput` copy of the arguments. Polished tools (terminal,
//! read_file, write_file, patch, search_files, web_search, …) deliberately omit
//! `rawInput`, so their operands are recovered from the deterministic title
//! prefixes that `build_tool_title` emits (`terminal: `, `read: `, `write: `,
//! `patch (mode): `, `search: `, `web search: `, `extract: `) and from
//! `locations[].path`. The `execute` kind additionally carries the FULL command
//! in its content block (`$ <cmd>`), which the title truncates at 80 chars —
//! so content wins for commands.
//!
//! Anything unrecognized degrades to [`ToolCall::Unknown`] carrying the title
//! and whatever `rawInput` was published, never to a dropped tool call.

use comet_proto::{AgentEvent, TodoItem, ToolCall};
use serde_json::Value;

/// The `sessionUpdate` discriminant of a `session/update` notification.
pub(crate) fn update_kind(params: &Value) -> &str {
    params
        .get("update")
        .and_then(|u| u.get("sessionUpdate"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub(crate) fn update(params: &Value) -> &Value {
    params.get("update").unwrap_or(&Value::Null)
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `update.content.text` — the chunk payload on message/thought updates.
/// Tolerates both a bare content object and the array form.
pub(crate) fn chunk_text(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    let text = match content {
        Value::Array(items) => items
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<String>(),
        other => other.get("text").and_then(Value::as_str)?.to_owned(),
    };
    (!text.is_empty()).then_some(text)
}

/// The first `locations[].path`, which Hermes fills for every file-shaped tool.
fn first_location(update: &Value) -> Option<String> {
    update
        .get("locations")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|l| l.get("path").and_then(Value::as_str))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

/// Concatenated text of the tool call's content blocks. Hermes wraps each block
/// as `{"type": "content", "content": {"type": "text", "text": …}}`.
fn content_text(update: &Value) -> String {
    let Some(items) = update.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|block| {
            block
                .get("content")
                .and_then(|c| c.get("text"))
                .or_else(|| block.get("text"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `rawInput`, published for every tool outside Hermes's polished set.
fn raw_input(update: &Value) -> Option<Value> {
    update.get("rawInput").filter(|v| !v.is_null()).cloned()
}

fn raw_str(raw: Option<&Value>, key: &str) -> Option<String> {
    raw?.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Split an `mcp__<server>__<tool>` name (Hermes's native MCP prefix, see
/// `tools/mcp_tool.py::mcp_prefixed_tool_name`) into its parts.
fn mcp_parts(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| (server.to_owned(), tool.to_owned()))
}

/// Map one ACP `tool_call` / `tool_call_update` payload to a typed [`ToolCall`].
pub(crate) fn tool_call(update: &Value) -> ToolCall {
    let kind = str_at(update, "kind");
    let title = str_at(update, "title");
    let raw = raw_input(update);
    let raw_ref = raw.as_ref();

    // MCP tools keep their prefixed name as the title (build_tool_title falls
    // through to `return tool_name`) and always publish rawInput.
    if let Some((server, tool)) = mcp_parts(title) {
        return ToolCall::Mcp {
            server,
            tool,
            input: raw,
        };
    }

    match kind {
        "execute" => {
            // Prefer the content block: `$ <cmd>` carries the untruncated
            // command, while the title clips at 80 chars.
            let content = content_text(update);
            let command = content
                .strip_prefix("$ ")
                .map(str::to_owned)
                .or_else(|| raw_str(raw_ref, "command"))
                .or_else(|| title.strip_prefix("terminal: ").map(str::to_owned))
                .unwrap_or_else(|| {
                    // execute_code ("python: …"), process, browser_* and any
                    // other execute-shaped tool: keep the rendered title.
                    raw_str(raw_ref, "code").unwrap_or_else(|| title.to_owned())
                });
            ToolCall::Exec { command }
        }

        "read" => match first_location(update)
            .or_else(|| raw_str(raw_ref, "path"))
            .or_else(|| title.strip_prefix("read: ").map(str::to_owned))
        {
            Some(path) => ToolCall::ReadFile { path },
            // skill_view / skills_list / browser_snapshot are read-kind but
            // pathless — they are not file reads.
            None => ToolCall::Unknown {
                name: title.to_owned(),
                input: raw,
            },
        },

        "edit" => {
            let path = first_location(update)
                .or_else(|| raw_str(raw_ref, "path"))
                .or_else(|| title.strip_prefix("write: ").map(str::to_owned));
            // `patch (<mode>): <path>` is an in-place edit; `write: <path>` a
            // whole-file write. Hermes attaches the diff as a content block,
            // which the render-parts policy strips anyway, so old/new stay None.
            if title.starts_with("patch") {
                return match path {
                    Some(path) => ToolCall::EditFile {
                        path,
                        old_string: None,
                        new_string: None,
                    },
                    None => ToolCall::ApplyPatch { path: None },
                };
            }
            match path {
                Some(path) => ToolCall::WriteFile {
                    path,
                    content: None,
                },
                None => ToolCall::Unknown {
                    name: title.to_owned(),
                    input: raw,
                },
            }
        }

        "search" => {
            let pattern = raw_str(raw_ref, "pattern")
                .or_else(|| title.strip_prefix("search: ").map(str::to_owned))
                .unwrap_or_else(|| title.to_owned());
            ToolCall::Search {
                pattern,
                path: first_location(update).or_else(|| raw_str(raw_ref, "path")),
            }
        }

        "fetch" => {
            if let Some(query) = raw_str(raw_ref, "query")
                .or_else(|| title.strip_prefix("web search: ").map(str::to_owned))
            {
                return ToolCall::WebSearch { query };
            }
            // web_extract renders `extract: <url> (+N)`; browser_navigate
            // renders `navigate: <url>`.
            let url = raw_str(raw_ref, "url")
                .or_else(|| {
                    title
                        .strip_prefix("extract: ")
                        .map(|u| u.split(" (+").next().unwrap_or(u).to_owned())
                })
                .or_else(|| title.strip_prefix("navigate: ").map(str::to_owned))
                .unwrap_or_else(|| title.to_owned());
            ToolCall::WebFetch { url, prompt: None }
        }

        // "other" / "think" / anything new: keep the rendered title and the
        // arguments Hermes published.
        _ => ToolCall::Unknown {
            name: title.to_owned(),
            input: raw,
        },
    }
}

/// A `tool_call_update` reports terminal status as `completed` / `failed`;
/// `pending` / `in_progress` are progress-only and resolve nothing.
pub(crate) fn tool_status(update: &Value) -> Option<bool> {
    match str_at(update, "status") {
        "completed" => Some(false),
        "failed" => Some(true),
        _ => None,
    }
}

pub(crate) fn tool_call_id(update: &Value) -> String {
    str_at(update, "toolCallId").to_owned()
}

/// ACP `plan` update → a Todo tool call. Entry status is one of
/// `pending` / `in_progress` / `completed`.
pub(crate) fn plan_items(update: &Value) -> Vec<TodoItem> {
    update
        .get("entries")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|e| TodoItem {
            text: str_at(e, "content").to_owned(),
            done: str_at(e, "status") == "completed",
        })
        .collect()
}

/// `session/prompt`'s response `usage` → a [`AgentEvent::Usage`] snapshot.
pub(crate) fn usage_event(result: &Value) -> Option<AgentEvent> {
    let usage = result.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
    Some(AgentEvent::Usage {
        input_tokens: count("inputTokens"),
        output_tokens: count("outputTokens"),
    })
}

/// The acknowledgements Hermes streams as ordinary assistant text when a
/// mid-turn prompt is absorbed by the running turn (`server.py`'s redirect and
/// queue branches). They are protocol chatter about OUR steer, not model
/// output: Comet already renders steering via [`AgentEvent::Steered`], so
/// echoing them would plant a stray line in the transcript.
pub(crate) fn is_steer_ack(text: &str) -> bool {
    let text = text.trim();
    text == "Redirected the active turn with your correction."
        || (text.starts_with("Queued for the next turn. (") && text.ends_with("queued)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Frames captured verbatim from a live `hermes acp` 0.19.1 run.
    #[test]
    fn live_terminal_frame_maps_to_exec() {
        let update = json!({
            "content": [{"content": {"text": "$ ls -la", "type": "text"}, "type": "content"}],
            "kind": "execute",
            "locations": [],
            "title": "terminal: ls -la",
            "toolCallId": "tc-503475423a98",
            "sessionUpdate": "tool_call"
        });
        assert_eq!(
            tool_call(&update),
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        assert_eq!(tool_call_id(&update), "tc-503475423a98");
    }

    /// The title truncates at 80 chars; the `$ ` content block does not.
    #[test]
    fn long_command_comes_from_content_not_truncated_title() {
        let long = "echo ".to_owned() + &"x".repeat(200);
        let update = json!({
            "content": [{"content": {"text": format!("$ {long}"), "type": "text"}, "type": "content"}],
            "kind": "execute",
            "title": "terminal: echo xxxxxxx...",
        });
        assert_eq!(tool_call(&update), ToolCall::Exec { command: long });
    }

    #[test]
    fn live_read_and_write_frames_map_to_file_calls() {
        let read = json!({
            "kind": "read",
            "locations": [{"path": "notes.txt"}],
            "title": "read: notes.txt",
            "toolCallId": "tc-f707daf50a8e",
            "sessionUpdate": "tool_call"
        });
        assert_eq!(
            tool_call(&read),
            ToolCall::ReadFile {
                path: "notes.txt".into()
            }
        );

        let write = json!({
            "content": [{"content": {"text": "Preparing write to notes.txt.", "type": "text"},
                         "type": "content"}],
            "kind": "edit",
            "locations": [{"path": "notes.txt"}],
            "title": "write: notes.txt",
            "toolCallId": "tc-1c849d4ec18e",
            "sessionUpdate": "tool_call"
        });
        assert_eq!(
            tool_call(&write),
            ToolCall::WriteFile {
                path: "notes.txt".into(),
                content: None
            }
        );
    }

    #[test]
    fn patch_maps_to_edit_and_pathless_patch_to_apply_patch() {
        let patch = json!({
            "kind": "edit",
            "locations": [{"path": "src/main.rs"}],
            "title": "patch (replace): src/main.rs",
        });
        assert_eq!(
            tool_call(&patch),
            ToolCall::EditFile {
                path: "src/main.rs".into(),
                old_string: None,
                new_string: None
            }
        );
        let pathless = json!({"kind": "edit", "title": "patch (replace): patch input"});
        assert_eq!(tool_call(&pathless), ToolCall::ApplyPatch { path: None });
    }

    #[test]
    fn search_and_web_kinds_map_to_typed_calls() {
        assert_eq!(
            tool_call(&json!({"kind": "search", "title": "search: TODO\\("})),
            ToolCall::Search {
                pattern: "TODO\\(".into(),
                path: None
            }
        );
        assert_eq!(
            tool_call(&json!({"kind": "fetch", "title": "web search: rust async"})),
            ToolCall::WebSearch {
                query: "rust async".into()
            }
        );
        assert_eq!(
            tool_call(&json!({"kind": "fetch", "title": "extract: https://a.dev/x (+2)"})),
            ToolCall::WebFetch {
                url: "https://a.dev/x".into(),
                prompt: None
            }
        );
        assert_eq!(
            tool_call(&json!({"kind": "fetch", "title": "navigate: https://b.dev"})),
            ToolCall::WebFetch {
                url: "https://b.dev".into(),
                prompt: None
            }
        );
    }

    /// Non-polished tools publish rawInput; MCP keeps its `mcp__server__tool`
    /// name as the title.
    #[test]
    fn mcp_tools_split_server_and_tool() {
        let update = json!({
            "kind": "other",
            "title": "mcp__github__create_issue",
            "rawInput": {"repo": "a/b", "title": "bug"},
        });
        assert_eq!(
            tool_call(&update),
            ToolCall::Mcp {
                server: "github".into(),
                tool: "create_issue".into(),
                input: Some(json!({"repo": "a/b", "title": "bug"})),
            }
        );
        // A malformed prefix is not an MCP call.
        assert!(matches!(
            tool_call(&json!({"kind": "other", "title": "mcp__nosep"})),
            ToolCall::Unknown { .. }
        ));
    }

    #[test]
    fn unknown_kinds_keep_title_and_raw_input() {
        let update = json!({
            "kind": "other",
            "title": "memory search: rust",
            "rawInput": {"action": "search"},
        });
        assert_eq!(
            tool_call(&update),
            ToolCall::Unknown {
                name: "memory search: rust".into(),
                input: Some(json!({"action": "search"})),
            }
        );
    }

    /// A read-kind tool with no path at all (skills_list) is not a file read.
    #[test]
    fn pathless_read_degrades_to_unknown() {
        assert!(matches!(
            tool_call(&json!({"kind": "read", "title": "skills list"})),
            ToolCall::Unknown { .. }
        ));
    }

    #[test]
    fn statuses_resolve_only_on_terminal_values() {
        assert_eq!(tool_status(&json!({"status": "completed"})), Some(false));
        assert_eq!(tool_status(&json!({"status": "failed"})), Some(true));
        assert_eq!(tool_status(&json!({"status": "pending"})), None);
        assert_eq!(tool_status(&json!({"status": "in_progress"})), None);
        assert_eq!(tool_status(&json!({})), None);
    }

    #[test]
    fn chunk_text_reads_object_and_array_content() {
        assert_eq!(
            chunk_text(&json!({"content": {"text": "hi", "type": "text"}})),
            Some("hi".into())
        );
        assert_eq!(
            chunk_text(&json!({"content": [{"text": "a"}, {"text": "b"}]})),
            Some("ab".into())
        );
        assert_eq!(chunk_text(&json!({"content": {"text": ""}})), None);
        assert_eq!(chunk_text(&json!({})), None);
    }

    #[test]
    fn plan_entries_map_to_todo_items() {
        let update = json!({"entries": [
            {"content": "Read the code", "status": "completed", "priority": "high"},
            {"content": "Write the fix", "status": "in_progress"},
        ]});
        assert_eq!(
            plan_items(&update),
            vec![
                TodoItem {
                    text: "Read the code".into(),
                    done: true
                },
                TodoItem {
                    text: "Write the fix".into(),
                    done: false
                },
            ]
        );
    }

    #[test]
    fn usage_reads_prompt_response_totals() {
        // Shape captured from a live session/prompt response.
        let result = json!({"stopReason": "end_turn", "usage": {
            "cachedReadTokens": 2432, "inputTokens": 16165, "outputTokens": 20,
            "thoughtTokens": 14, "totalTokens": 16185}});
        assert_eq!(
            usage_event(&result),
            Some(AgentEvent::Usage {
                input_tokens: 16165,
                output_tokens: 20
            })
        );
        // A steer ack carries no usage at all.
        assert_eq!(usage_event(&json!({"stopReason": "end_turn"})), None);
    }

    #[test]
    fn steer_acks_are_recognized_but_real_text_is_not() {
        assert!(is_steer_ack(
            "Redirected the active turn with your correction."
        ));
        assert!(is_steer_ack("Queued for the next turn. (2 queued)"));
        assert!(!is_steer_ack("Redirecting stdout to a file is easy."));
        assert!(!is_steer_ack("Done."));
    }

    #[test]
    fn update_kind_reads_the_discriminant() {
        let params = json!({"sessionId": "s", "update": {"sessionUpdate": "agent_message_chunk"}});
        assert_eq!(update_kind(&params), "agent_message_chunk");
        assert_eq!(update_kind(&json!({})), "");
    }
}
