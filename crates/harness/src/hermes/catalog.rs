//! Model catalog + session-mode mapping for Hermes.
//!
//! Unlike Codex (curated snapshot), Hermes's model list is LIVE: it is whatever
//! providers the user has authenticated on this device, so the catalog is
//! discovered from `session/new`'s `models.availableModels` rather than
//! hardcoded. Model ids are provider-qualified (`xai-oauth:grok-4.5`) and are
//! passed back verbatim to `session/set_model`.

use comet_proto::{Model, ReasoningLevel, SandboxLevel};
use serde_json::Value;

/// Hermes exposes no reasoning-effort control over ACP — effort is a property
/// of the selected provider/model, chosen in `hermes model`, not a per-turn
/// knob. Advertising an empty ladder keeps the composer from offering a
/// setting the harness would silently drop.
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[];

/// Hermes's ACP session modes are its edit-approval policy (`_MODE_*` in
/// `acp_adapter/server.py`). Comet's sandbox level plus `auto_approve` pick one:
///
/// - `default` — ask before edits (read-only runs never write anyway)
/// - `accept_edits` — auto-allow workspace and /tmp edits, still ask for
///   sensitive paths (the workspace-write default)
/// - `dont_ask` — auto-allow every file edit except sensitive paths
pub(crate) fn session_mode(sandbox: SandboxLevel, auto_approve: bool) -> &'static str {
    if auto_approve {
        return "dont_ask";
    }
    match sandbox {
        SandboxLevel::ReadOnly => "default",
        SandboxLevel::WorkspaceWrite => "accept_edits",
        SandboxLevel::DangerFullAccess => "dont_ask",
    }
}

/// Parse `models.availableModels` from a `session/new` / `session/load`
/// response into Comet's catalog shape. Entries without a `modelId` are
/// dropped; `name`/`description` degrade to the id when absent.
pub(crate) fn models_from_session(result: &Value) -> Vec<Model> {
    let Some(available) = result
        .get("models")
        .and_then(|m| m.get("availableModels"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    available
        .iter()
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            if id.is_empty() {
                return None;
            }
            let label = m
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            let description = m
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Some(Model {
                id: id.to_owned(),
                label: label.to_owned(),
                description,
                reasoning_levels: Vec::new(),
                options: Vec::new(),
            })
        })
        .collect()
}

/// `models.currentModelId` — the provider/model pair Hermes booted with, used
/// to skip a redundant `session/set_model`.
pub(crate) fn current_model(result: &Value) -> Option<String> {
    result
        .get("models")
        .and_then(|m| m.get("currentModelId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// ACP `stopReason` → whether the turn ran to completion. `cancelled` is the
/// only interrupted outcome; `refusal` and the `max_*` limits are ordinary
/// turn ends whose explanation already streamed as assistant text.
pub(crate) fn stop_reason_interrupted(stop_reason: &str) -> bool {
    stop_reason == "cancelled"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn modes_follow_sandbox_and_auto_approve() {
        assert_eq!(session_mode(SandboxLevel::ReadOnly, false), "default");
        assert_eq!(
            session_mode(SandboxLevel::WorkspaceWrite, false),
            "accept_edits"
        );
        assert_eq!(
            session_mode(SandboxLevel::DangerFullAccess, false),
            "dont_ask"
        );
        // auto_approve overrides every sandbox level.
        assert_eq!(session_mode(SandboxLevel::ReadOnly, true), "dont_ask");
    }

    #[test]
    fn models_parse_from_live_session_payload() {
        let result = json!({
            "sessionId": "s1",
            "models": {
                "currentModelId": "xai-oauth:grok-4.5",
                "availableModels": [
                    {"modelId": "xai-oauth:grok-4.5", "name": "xAI · grok-4.5",
                     "description": "Provider: xAI"},
                    {"modelId": "openai-codex:gpt-5.5", "name": "OpenAI Codex · gpt-5.5"},
                    {"name": "no id — dropped"},
                ]
            }
        });
        let models = models_from_session(&result);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "xai-oauth:grok-4.5");
        assert_eq!(models[0].label, "xAI · grok-4.5");
        assert_eq!(models[0].description.as_deref(), Some("Provider: xAI"));
        // A missing description stays None rather than echoing the label.
        assert_eq!(models[1].description, None);
        // Hermes has no per-turn effort control.
        assert!(models[0].reasoning_levels.is_empty());
        assert_eq!(
            current_model(&result).as_deref(),
            Some("xai-oauth:grok-4.5")
        );
        assert_eq!(current_model(&json!({})), None);
    }

    #[test]
    fn only_cancelled_reads_as_interrupted() {
        assert!(stop_reason_interrupted("cancelled"));
        assert!(!stop_reason_interrupted("end_turn"));
        assert!(!stop_reason_interrupted("refusal"));
        assert!(!stop_reason_interrupted("max_tokens"));
    }
}
