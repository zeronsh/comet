//! Claude Code user settings — specifically the `env` block that cc-switch and
//! Anthropic-compatible gateways rewrite to point the CLI at a third-party
//! provider (issue #305).
//!
//! The CLI resolves the `ANTHROPIC_DEFAULT_{FABLE,OPUS,SONNET,HAIKU}_MODEL`
//! mapping **only for family aliases** (`fable`, `opus`, `sonnet`, `haiku`).
//! A full model id passed via `--model` (`claude-opus-5`) goes to the provider
//! verbatim, so the mapping is bypassed. Zeron's curated catalog is full ids,
//! which is exactly that case — hence this reader: `build_command` resolves the
//! selected id through the configured family mapping, and `models()` surfaces
//! the provider models so the picker names what actually runs.
//!
//! Only `settings.json` under the Claude config dir is read. The CLI merges
//! more sources (project/local settings, process env), but the user-level file
//! is what cc-switch writes and the common case; process env is inherited by
//! the spawned CLI anyway.

use std::path::PathBuf;

use serde_json::Value;

/// One of the four Claude model families the CLI maps through
/// `ANTHROPIC_DEFAULT_<FAMILY>_MODEL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl Family {
    /// Every family, in flagship order — the picker's dedup order.
    pub(crate) const ALL: [Family; 4] =
        [Family::Fable, Family::Opus, Family::Sonnet, Family::Haiku];

    /// The alias the CLI accepts on `--model` and resolves to the family's
    /// configured provider model.
    pub(crate) fn alias(self) -> &'static str {
        match self {
            Family::Fable => "fable",
            Family::Opus => "opus",
            Family::Sonnet => "sonnet",
            Family::Haiku => "haiku",
        }
    }

    fn model_key(self) -> &'static str {
        match self {
            Family::Fable => "ANTHROPIC_DEFAULT_FABLE_MODEL",
            Family::Opus => "ANTHROPIC_DEFAULT_OPUS_MODEL",
            Family::Sonnet => "ANTHROPIC_DEFAULT_SONNET_MODEL",
            Family::Haiku => "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        }
    }

    fn name_key(self) -> &'static str {
        match self {
            Family::Fable => "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
            Family::Opus => "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            Family::Sonnet => "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            Family::Haiku => "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        }
    }
}

/// The family a curated catalog id belongs to (`claude-opus-5` → `Opus`).
/// Substring match, mirroring [`super::catalog::supports_xhigh`]'s approach.
pub(crate) fn family_of(model: &str) -> Option<Family> {
    let model = model.to_ascii_lowercase();
    ["fable", "opus", "sonnet", "haiku"]
        .into_iter()
        .find(|needle| model.contains(*needle))
        .and_then(|needle| {
            Family::ALL
                .into_iter()
                .find(|family| family.alias() == needle)
        })
}

/// A provider model configured for one family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderModel {
    pub family: Family,
    /// The value the CLI maps the family alias to (`ANTHROPIC_DEFAULT_*_MODEL`,
    /// e.g. `deepseek-v4-flash[1M]`). Safe to pass as `--model` directly — the
    /// CLI strips the trailing context suffix.
    pub model: String,
    /// Display name (`ANTHROPIC_DEFAULT_*_MODEL_NAME`), falling back to the
    /// model value with any `[...]` suffix removed.
    pub label: String,
}

/// The provider-facing slice of Claude Code's user settings.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeSettings {
    /// Per-family provider mappings. Empty when no third-party mapping is
    /// configured (native Anthropic users), which leaves the curated catalog
    /// and `--model` behavior untouched.
    pub families: Vec<ProviderModel>,
}

impl ClaudeSettings {
    /// Read the user-level settings file, tolerating every failure (missing
    /// file, bad JSON, no Claude install). A read error is indistinguishable
    /// from "no provider mapping", which is the safe default.
    pub(crate) fn load() -> Self {
        Self::load_from(&settings_path())
    }

    fn load_from(path: &std::path::Path) -> Self {
        let Ok(source) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Value>(&source)
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

    /// Parse the `env` block. Visible for tests.
    pub(crate) fn from_json(value: &Value) -> Self {
        let env = value.get("env").and_then(Value::as_object);
        let string = |key: &str| {
            env.and_then(|env| env.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let base_url = string("ANTHROPIC_BASE_URL");
        let default_model = string("ANTHROPIC_MODEL");
        let mut families = Vec::new();
        for family in Family::ALL {
            let label = string(family.name_key());
            let mapping = match string(family.model_key()) {
                Some(model) => Some((model, label.unwrap_or_default())),
                // A gateway that only pins `ANTHROPIC_MODEL` (older cc-switch)
                // still needs its model to reach every family.
                None if base_url.is_some() => default_model
                    .clone()
                    .map(|model| (model, label.unwrap_or_default())),
                None => None,
            };
            if let Some((model, label)) = mapping {
                let label = if label.is_empty() {
                    display_name(&model)
                } else {
                    label
                };
                families.push(ProviderModel {
                    family,
                    model,
                    label,
                });
            }
        }
        Self { families }
    }

    /// Whether a third-party family mapping is active at all.
    pub(crate) fn has_provider_mapping(&self) -> bool {
        !self.families.is_empty()
    }

    pub(crate) fn family(&self, family: Family) -> Option<&ProviderModel> {
        self.families.iter().find(|pm| pm.family == family)
    }

    /// Resolve a picker/catalog model id to the value `--model` should carry.
    ///
    /// `None` means "no provider mapping applies — use the id as-is", which
    /// keeps native Anthropic behavior (including the `[1m]` context suffix)
    /// exactly as it was.
    pub(crate) fn resolve_request_model(&self, model: &str) -> Option<String> {
        // A provider row already carries the provider model value; pass it on
        // untouched (the CLI strips a `[...]` suffix).
        if self.families.iter().any(|pm| pm.model == model) {
            return Some(model.to_owned());
        }
        family_of(model)
            .and_then(|family| self.family(family))
            .map(|pm| pm.model.clone())
    }

    /// Picker rows for the configured provider models, deduped by display name
    /// (cc-switch points every family at one model, which would otherwise show
    /// as four identical rows). `None` when no mapping is configured.
    pub(crate) fn provider_rows(
        &self,
        catalog: &[zeron_proto::Model],
    ) -> Option<Vec<zeron_proto::Model>> {
        if !self.has_provider_mapping() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for pm in &self.families {
            if !seen.insert(dedup_key(&pm.label)) {
                continue;
            }
            // Keep the family's curated effort ladder usable; curated options
            // (context window / fast mode) are Claude-native and dropped.
            let base = catalog
                .iter()
                .find(|model| family_of(&model.id) == Some(pm.family));
            rows.push(zeron_proto::Model {
                id: pm.model.clone(),
                label: pm.label.clone(),
                description: None,
                reasoning_levels: base
                    .map(|model| model.reasoning_levels.clone())
                    .unwrap_or_default(),
                options: Vec::new(),
            });
        }
        Some(rows)
    }
}

/// Claude config dir (`$CLAUDE_CONFIG_DIR` else `~/.claude`), matching
/// `zeron_engine`'s account resolution.
fn settings_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("settings.json");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude")
        .join("settings.json")
}

/// `deepseek-v4-flash[1M]` → `deepseek-v4-flash`.
fn display_name(model: &str) -> String {
    model
        .split_once('[')
        .map(|(base, _)| base)
        .unwrap_or(model)
        .trim()
        .to_owned()
}

/// Dedup key for provider rows: the display name, case/suffix-insensitive.
fn dedup_key(label: &str) -> String {
    display_name(label).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(env: Value) -> ClaudeSettings {
        ClaudeSettings::from_json(&json!({ "env": env }))
    }

    #[test]
    fn family_matching_is_substring_like_the_xhigh_check() {
        assert_eq!(family_of("claude-opus-5"), Some(Family::Opus));
        assert_eq!(family_of("claude-opus-4-7-20260101"), Some(Family::Opus));
        assert_eq!(family_of("claude-sonnet-5[1m]"), Some(Family::Sonnet));
        assert_eq!(family_of("claude-haiku-4-5"), Some(Family::Haiku));
        assert_eq!(family_of("claude-fable-5-1"), Some(Family::Fable));
        assert_eq!(family_of("gpt-5.2-codex"), None);
    }

    #[test]
    fn cc_switch_default_family_models_map_each_family() {
        let settings = settings(json!({
            "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
            "ANTHROPIC_MODEL": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "deepseek-v4-flash[1M]",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-flash[1M]",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_FABLE_MODEL": "deepseek-v4-flash[1M]",
        }));
        assert!(settings.has_provider_mapping());
        assert_eq!(
            settings.resolve_request_model("claude-opus-5").as_deref(),
            Some("deepseek-v4-flash[1M]")
        );
        assert_eq!(
            settings
                .resolve_request_model("claude-haiku-4-5")
                .as_deref(),
            Some("deepseek-v4-flash")
        );
        // Provider row ids pass through untouched.
        assert_eq!(
            settings
                .resolve_request_model("deepseek-v4-flash[1M]")
                .as_deref(),
            Some("deepseek-v4-flash[1M]")
        );
    }

    #[test]
    fn non_claude_ids_are_never_remapped() {
        let settings = settings(json!({
            "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "deepseek-v4-flash[1M]",
        }));
        assert_eq!(settings.resolve_request_model("gpt-5.2-codex"), None);
    }

    #[test]
    fn native_settings_leave_everything_alone() {
        let settings = settings(json!({ "ANTHROPIC_MODEL": "claude-opus-5" }));
        assert!(!settings.has_provider_mapping());
        assert_eq!(settings.resolve_request_model("claude-opus-5"), None);
        assert!(
            settings
                .provider_rows(&super::super::catalog::static_models())
                .is_none()
        );
    }

    #[test]
    fn a_model_only_gateway_falls_back_to_anthropic_model() {
        let settings = settings(json!({
            "ANTHROPIC_BASE_URL": "https://gateway.example/anthropic",
            "ANTHROPIC_MODEL": "glm-5.2",
        }));
        assert_eq!(
            settings.resolve_request_model("claude-sonnet-5").as_deref(),
            Some("glm-5.2")
        );
        // All four families carry the same fallback, deduped to one row by name.
        let rows = settings
            .provider_rows(&super::super::catalog::static_models())
            .expect("provider rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "glm-5.2");
        assert_eq!(rows[0].label, "glm-5.2");
    }

    #[test]
    fn provider_rows_dedup_by_display_name() {
        let settings = settings(json!({
            "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "deepseek-v4-flash[1M]",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-flash[1M]",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "deepseek-v4-flash",
            "ANTHROPIC_DEFAULT_FABLE_MODEL": "deepseek-v4-flash[1M]",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME": "deepseek-v4-flash",
        }));
        let rows = settings
            .provider_rows(&super::super::catalog::static_models())
            .expect("provider rows");
        assert_eq!(rows.len(), 1, "same provider model must collapse: {rows:?}");
        assert_eq!(rows[0].id, "deepseek-v4-flash[1M]");
        assert_eq!(rows[0].label, "deepseek-v4-flash");
        assert!(!rows[0].reasoning_levels.is_empty());
    }

    #[test]
    fn distinct_provider_models_stay_separate() {
        let settings = settings(json!({
            "ANTHROPIC_BASE_URL": "https://gateway.example/anthropic",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "big-model",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "small-model",
        }));
        let rows = settings
            .provider_rows(&super::super::catalog::static_models())
            .expect("provider rows");
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["big-model", "small-model"]);
    }
}
