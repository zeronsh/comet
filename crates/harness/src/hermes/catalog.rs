//! Model catalog + session-mode mapping for Hermes.
//!
//! Unlike Codex (curated snapshot), Hermes's model list is LIVE: it is whatever
//! providers the user has authenticated on this device, so the catalog is
//! discovered from `session/new`'s `models.availableModels` rather than
//! hardcoded. Model ids are provider-qualified (`xai-oauth:grok-4.5`) and are
//! passed back verbatim to `session/set_model`. When the underlying model is
//! also offered by Comet's Codex or Claude harness, its picker traits are
//! overlaid onto the live row so changing harness does not hide model controls.

use comet_proto::{Model, ModelOption, ReasoningLevel, SandboxLevel};
use serde_json::Value;

/// There is no harness-wide ladder: Hermes is provider-agnostic, so each live
/// model row receives the underlying Codex/Claude model's own ladder instead.
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[];

/// The provider-qualified ACP id's model half. OpenRouter-style ids can retain
/// a namespace (`openrouter:anthropic/claude-opus-5`), so callers also try the
/// final path component when matching Comet's curated catalogs.
fn model_id_candidates(id: &str) -> impl Iterator<Item = &str> {
    let model = id.split_once(':').map_or(id, |(_, model)| model);
    [model, model.rsplit('/').next().unwrap_or(model)].into_iter()
}

fn shared_harness_traits(id: &str) -> Option<(Vec<ReasoningLevel>, Vec<ModelOption>)> {
    let candidates: Vec<&str> = model_id_candidates(id).collect();
    crate::codex::catalog::static_models()
        .into_iter()
        .chain(crate::claude::catalog::static_models())
        .find(|model| candidates.iter().any(|candidate| *candidate == model.id))
        .map(|model| {
            // Hermes's provider-facing effort vocabulary is the seven ordinary
            // levels used by its desktop app. Claude Code's prompt/settings
            // special modes are harness-specific and cannot cross ACP.
            let reasoning_levels = model
                .reasoning_levels
                .into_iter()
                .filter(|level| {
                    !matches!(
                        level,
                        ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink
                    )
                })
                .collect();
            // The Hermes desktop model menu exposes reasoning + fast. Do not
            // leak Claude Code-only context-window settings into this harness.
            let options = model
                .options
                .into_iter()
                .filter(|option| matches!(option.id.as_str(), "serviceTier" | "fastMode"))
                .collect();
            (reasoning_levels, options)
        })
}

/// Hermes's generic reasoning setting accepts the shared effort vocabulary.
/// Harness-specific special modes degrade to their underlying xhigh effort.
pub(crate) fn reasoning_effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    }
}

/// Normalize Comet's Codex/Claude speed controls to Hermes's per-session fast
/// setting. Hermes names the provider request value `priority`.
pub(crate) fn service_tier(
    model_id: Option<&str>,
    options: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    let supports_fast = model_id
        .and_then(shared_harness_traits)
        .is_some_and(|(_, options)| !options.is_empty());
    if !supports_fast {
        return None;
    }
    let codex_fast = options.get("serviceTier").and_then(Value::as_str) == Some("fast");
    let claude_fast = options.get("fastMode").and_then(Value::as_str) == Some("on");
    Some(if codex_fast || claude_fast {
        "priority"
    } else {
        "normal"
    })
}

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
            let (reasoning_levels, options) =
                shared_harness_traits(id).unwrap_or_else(|| (Vec::new(), Vec::new()));
            Some(Model {
                id: id.to_owned(),
                label: label.to_owned(),
                description,
                reasoning_levels,
                options,
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
                    {"modelId": "anthropic:claude-opus-5", "name": "Anthropic · claude-opus-5"},
                    {"name": "no id — dropped"},
                ]
            }
        });
        let models = models_from_session(&result);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "xai-oauth:grok-4.5");
        assert_eq!(models[0].label, "xAI · grok-4.5");
        assert_eq!(models[0].description.as_deref(), Some("Provider: xAI"));
        // A missing description stays None rather than echoing the label.
        assert_eq!(models[1].description, None);
        // Non-Claude/Codex providers retain the raw ACP catalog traits.
        assert!(models[0].reasoning_levels.is_empty());
        // Provider-qualified models carry the same traits Comet exposes for
        // the underlying Codex / Claude harness model.
        assert!(models[1].reasoning_levels.contains(&ReasoningLevel::XHigh));
        assert!(models[1].options.iter().any(|o| o.id == "serviceTier"));
        assert!(models[2].reasoning_levels.contains(&ReasoningLevel::Max));
        assert!(
            !models[2]
                .reasoning_levels
                .contains(&ReasoningLevel::Ultrathink)
        );
        assert!(models[2].options.iter().any(|o| o.id == "fastMode"));
        assert!(!models[2].options.iter().any(|o| o.id == "contextWindow"));
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

    #[test]
    fn run_traits_normalize_to_hermes_config_values() {
        assert_eq!(reasoning_effort(ReasoningLevel::Minimal), "minimal");
        assert_eq!(reasoning_effort(ReasoningLevel::Ultrathink), "xhigh");
        assert_eq!(
            service_tier(Some("xai-oauth:grok-4.5"), &serde_json::Map::new()),
            None
        );
        assert_eq!(
            service_tier(Some("openai-codex:gpt-5.5"), &serde_json::Map::new()),
            Some("normal")
        );
        assert_eq!(
            service_tier(
                Some("openai-codex:gpt-5.5"),
                &serde_json::from_value(json!({"serviceTier": "fast"})).unwrap()
            ),
            Some("priority")
        );
        assert_eq!(
            service_tier(
                Some("anthropic:claude-opus-5"),
                &serde_json::from_value(json!({"fastMode": "on"})).unwrap()
            ),
            Some("priority")
        );
    }
}
