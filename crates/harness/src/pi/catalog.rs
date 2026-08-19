//! Pi model catalog — reads `models-store.json` from the Pi agent data dir
//! and builds the [`Model`] list for the picker.
//!
//! Mirrors bb's `packages/agent-runtime/src/pi/bridge/model-list.ts` and
//! `packages/agent-runtime/src/pi/model-list.ts`, adapted for Rust: reads the
//! same on-disk store Pi's own SDK writes, so the picker always reflects the
//! user's configured providers and extensions without spawning a process.

use serde::Deserialize;
use zeron_proto::{Model, ReasoningLevel};

/// A single model entry inside `models-store.json`.
#[derive(Debug, Deserialize)]
struct StoreModel {
    id: String,
    name: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    #[serde(rename = "thinkingLevelMap")]
    thinking_level_map: serde_json::Map<String, serde_json::Value>,
}

/// One provider's worth of models inside `models-store.json`.
#[derive(Debug, Deserialize)]
struct ProviderModels {
    #[serde(default)]
    models: Vec<StoreModel>,
}

/// Full contents of `models-store.json`: a map from provider id → models.
type ModelsStore = std::collections::HashMap<String, ProviderModels>;

/// Model IDs ending with a `-YYYYMMDD` date suffix are pinned versions; we
/// exclude them from the picker and surface aliases only (bb convention from
/// Pi's `isAlias` heuristic in model-resolver.ts).
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    // "-YYYYMMDD" = 9 chars: dash + 8 digits
    if id.len() < 9 {
        return true;
    }
    let suffix = &id[id.len() - 9..];
    if !suffix.starts_with('-') {
        return true;
    }
    !suffix[1..].chars().all(|c| c.is_ascii_digit())
}

/// Map a Pi thinking-level key to Zeron's [`ReasoningLevel`]. `off` maps to
/// `None` — Pi's "no extended thinking" has no Zeron equivalent (Zeron's
/// ladder starts at `Minimal`). bb maps it to `"none"`.
fn reasoning_from_pi_level(key: &str) -> Option<ReasoningLevel> {
    match key {
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "xhigh" => Some(ReasoningLevel::XHigh),
        "max" => Some(ReasoningLevel::Max),
        _ => None,
    }
}

/// Derive the supported reasoning ladder from the model's `thinkingLevelMap`
/// keys. A model supports a level when the key is present (value may be null).
fn model_ladder(thinking_level_map: &serde_json::Map<String, serde_json::Value>) -> Vec<ReasoningLevel> {
    let mut levels: Vec<ReasoningLevel> = thinking_level_map
        .keys()
        .filter_map(|k| reasoning_from_pi_level(k))
        .collect();
    levels.sort_by_key(|l| *l as u8);
    levels
}

/// Build a description string like "OpenAI Codex reasoning, multimodal model via Pi".
fn describe(model: &StoreModel) -> String {
    let capabilities: Vec<&str> = if model.reasoning {
        vec!["reasoning"]
    } else {
        vec!["non-reasoning"]
    };
    let provider_display = if model.provider.is_empty() {
        "Pi".to_string()
    } else {
        let mut chars = model.provider.chars();
        match chars.next() {
            None => "Pi".to_string(),
            Some(first) => {
                let rest: String = chars.collect();
                format!("{}{rest}", first.to_ascii_uppercase())
            }
        }
    };
    let mut desc = provider_display;
    desc.push_str(" ");
    desc.push_str(&capabilities.join(", "));
    if model.input.contains(&"image".to_string()) {
        desc.push_str(", multimodal");
    }
    desc.push_str(" model via Pi");
    desc
}

/// Canonical model id: `provider/modelId` (bb convention). Consumers split on
/// the FIRST slash only — model ids may contain their own slashes (OpenRouter).
fn canonical_id(provider: &str, model_id: &str) -> String {
    format!("{provider}/{model_id}")
}

/// Read and parse the Pi agent models store. Returns models from every
/// configured provider, filtered to alias-only (no dated pinned versions).
pub fn load_pi_models() -> Vec<Model> {
    let store_path = pi_agent_dir().join("models-store.json");

    let json = match std::fs::read_to_string(&store_path) {
        Ok(json) => json,
        Err(_) => {
            tracing::warn!(
                target: "zeron_harness::pi",
                path = %store_path.display(),
                "pi models-store.json not found; falling back to \"pi default\""
            );
            return fallback_models();
        }
    };

    let store: ModelsStore = match serde_json::from_str(&json) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "zeron_harness::pi",
                path = %store_path.display(),
                error = %e,
                "failed to parse pi models-store.json"
            );
            return fallback_models();
        }
    };

    let mut models: Vec<Model> = Vec::new();

    for (provider, provider_models) in &store {
        for entry in &provider_models.models {
            // Skip dated pinned versions — only aliases go in the picker.
            if !is_alias(&entry.id) {
                continue;
            }
            let id = canonical_id(provider, &entry.id);
            let ladder = model_ladder(&entry.thinking_level_map);

            // Non-reasoning models with no thinking levels get an empty
            // ladder; reasoning models with an empty map get the default set.
            let reasoning_levels = if ladder.is_empty() && entry.reasoning {
                vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                    ReasoningLevel::Max,
                ]
            } else {
                ladder
            };

            models.push(Model {
                id,
                label: entry.name.clone(),
                description: Some(describe(entry)),
                reasoning_levels,
                options: Vec::new(),
            });
        }
    }

    if models.is_empty() {
        tracing::warn!(
            target: "zeron_harness::pi",
            "pi models-store.json contained no alias models"
        );
        return fallback_models();
    }

    // Sort: default models first (per-provider best picks), then the rest
    // alphabetically by label.
    models.sort_by(|a, b| {
        let a_def = is_default_pi_model(&a.id);
        let b_def = is_default_pi_model(&b.id);
        b_def
            .cmp(&a_def)
            .then_with(|| a.label.cmp(&b.label))
    });

    models
}

/// Per-provider default model ids (subset of Pi's own `defaultModelPerProvider`).
/// A model whose canonical id matches is sorted to the top of the picker.
fn is_default_pi_model(canonical: &str) -> bool {
    // canonical = "provider/modelId"
    let (provider, model_id) = match canonical.split_once('/') {
        Some(p) => p,
        None => return false,
    };
    matches!(
        (provider, model_id),
        ("anthropic", "claude-opus-4-8")
            | ("openai", "gpt-5.4")
            | ("openai-codex", "gpt-5.6-sol")
            | ("google", "gemini-2.5-pro")
            | ("google-gemini-cli", "gemini-2.5-pro")
            | ("google-vertex", "gemini-3-pro-preview")
            | ("openrouter", "openai/gpt-5.1-codex")
            | ("vercel-ai-gateway", "anthropic/claude-opus-4.8")
            | ("xai", "grok-4-fast-non-reasoning")
            | ("mistral", "devstral-medium-latest")
    )
}

/// The single-model fallback when the store can't be read (mirroring the
/// original hardcoded catalog).
fn fallback_models() -> Vec<Model> {
    vec![Model {
        id: "default".into(),
        label: "pi default".into(),
        description: Some("Runs the model configured in pi (`pi` settings)".into()),
        reasoning_levels: vec![
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ],
        options: Vec::new(),
    }]
}

/// Resolve the Pi agent data directory from `PI_AGENT_DIR` or the default
/// `~/.pi/agent`.
pub fn pi_agent_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("PI_AGENT_DIR").filter(|d| !d.is_empty()) {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    std::path::PathBuf::from(home).join(".pi").join("agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_detection() {
        assert!(is_alias("gpt-5.4"));
        assert!(is_alias("gpt-5.4-latest"));
        assert!(is_alias("claude-opus-5"));
        assert!(!is_alias("gpt-5.4-20250501"));  // dated
        assert!(!is_alias("claude-opus-4-7-20260101"));  // dated
        // Short ids that can't have a date suffix are aliases.
        assert!(is_alias("gpt"));
        assert!(is_alias("a-2025"));  // 4 digit suffix = not a date
    }

    #[test]
    fn canonical_ids() {
        assert_eq!(canonical_id("openai", "gpt-5.4"), "openai/gpt-5.4");
        assert_eq!(
            canonical_id("openrouter", "openai/gpt-5.1-codex"),
            "openrouter/openai/gpt-5.1-codex"
        );
    }

    #[test]
    fn level_mapping() {
        assert_eq!(reasoning_from_pi_level("off"), None);
        assert_eq!(reasoning_from_pi_level("minimal"), Some(ReasoningLevel::Minimal));
        assert_eq!(reasoning_from_pi_level("low"), Some(ReasoningLevel::Low));
        assert_eq!(reasoning_from_pi_level("medium"), Some(ReasoningLevel::Medium));
        assert_eq!(reasoning_from_pi_level("high"), Some(ReasoningLevel::High));
        assert_eq!(reasoning_from_pi_level("xhigh"), Some(ReasoningLevel::XHigh));
        assert_eq!(reasoning_from_pi_level("max"), Some(ReasoningLevel::Max));
        assert_eq!(reasoning_from_pi_level("ultra"), None);
    }

    #[test]
    fn ladder_from_map() {
        let map: serde_json::Map<String, serde_json::Value> = [
            ("xhigh".into(), serde_json::Value::Null),
            ("max".into(), serde_json::Value::Null),
            ("minimal".into(), serde_json::json!("low")),
        ]
        .into_iter()
        .collect();
        let ladder = model_ladder(&map);
        assert_eq!(
            ladder,
            vec![ReasoningLevel::Minimal, ReasoningLevel::XHigh, ReasoningLevel::Max]
        );
    }

    #[test]
    fn fallback_is_non_empty() {
        let models = fallback_models();
        assert!(!models.is_empty());
        assert_eq!(models[0].id, "default");
    }

}

