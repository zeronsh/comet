//! HarnessRegistry — the engine's harness catalog: eager instances (mock) plus lazy
//! slots resolved on first use (claude-code spawns subprocess discovery; codex/cursor
//! later). Lazy slots carry a static descriptor so `ListHarnesses` never forces a spawn.
//!
//! Also owns the device's harness ENABLEMENT (Settings → Agents): which harnesses
//! this device's composer offers, persisted in `{data_dir}/harness-prefs.json`.
//! Per-device because CLI installs are — a viewer retargets the settings page at
//! another device and edits THAT device's set over the forwarded RPCs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use zeron_harness::{Harness, HarnessError, mock::MockHarness};
use zeron_proto::{AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, SteeringMode};

/// How long a previously-discovered model list is served without re-probing.
/// Long enough that every app launch (the common case) skips the cold agent
/// boot entirely — opencode's Node startup measured ~13s under the app's
/// 4-way boot prefetch — while still re-discovering within a day.
const MODELS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// A fresh cache entry is re-probed in the background at most this often, so
/// the file stays warm for the next launch without a probe per picker open.
const MODELS_CACHE_REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);

/// One harness's persisted model discovery (`{data_dir}/model-cache.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedModels {
    discovered_at_ms: i64,
    models: Vec<Model>,
}

/// The persisted shape of `model-cache.json`, keyed by harness id.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ModelsCacheFile {
    entries: HashMap<HarnessId, CachedModels>,
}

/// What `ListHarnesses` reports per harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub name: String,
    pub supports_steering: bool,
    pub steering_mode: SteeringMode,
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// Whether the agent's CLI is present on the listing device (the settings
    /// enable-gate). Defaults true so catalogs from engines predating the
    /// field never read as uninstallable.
    #[serde(default = "default_installed")]
    pub installed: bool,
    /// Whether the listing device offers this harness (Settings → Agents).
    /// `None` — the catalog came from an engine predating the setting — means
    /// "unknown": consumers fall back to [`default_enabled`] membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn default_installed() -> bool {
    true
}

/// The out-of-the-box enabled set: Claude Code and Codex only; every other
/// harness is opt-in from Settings → Agents.
pub fn default_enabled() -> Vec<HarnessId> {
    vec![HarnessId::ClaudeCode, HarnessId::Codex]
}

/// A descriptor's effective enabled flag ([`default_enabled`] membership when
/// the catalog predates the setting).
pub fn descriptor_enabled(descriptor: &HarnessDescriptor) -> bool {
    descriptor
        .enabled
        .unwrap_or_else(|| default_enabled().contains(&descriptor.id))
}

fn describe(harness: &dyn Harness) -> HarnessDescriptor {
    HarnessDescriptor {
        id: harness.id(),
        name: harness.display_name().to_string(),
        supports_steering: harness.supports_steering(),
        steering_mode: harness.steering_mode(),
        reasoning_levels: harness.reasoning_levels().to_vec(),
        installed: harness.installed(),
        enabled: None,
    }
}

/// The persisted shape of `harness-prefs.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HarnessPrefsFile {
    /// `None` = the user never touched the setting → the default set.
    enabled: Option<Vec<HarnessId>>,
}

type Factory = Box<dyn Fn() -> Result<Arc<dyn Harness>, HarnessError> + Send + Sync>;
type InstalledProbe = Box<dyn Fn() -> bool + Send + Sync>;

enum Slot {
    Ready(Arc<dyn Harness>),
    Lazy {
        descriptor: HarnessDescriptor,
        /// Re-run on every `descriptors()` call — a CLI installed mid-session
        /// shows up on the next settings/picker open, no restart needed.
        installed: InstalledProbe,
        factory: Factory,
    },
}

pub struct HarnessRegistry {
    slots: Mutex<HashMap<HarnessId, Slot>>,
    order: Mutex<Vec<HarnessId>>,
    /// This device's enabled set; `None` inner value = the default set.
    prefs: Mutex<HarnessPrefsFile>,
    /// Where the prefs persist; `None` (tests, bare registries) skips writes.
    prefs_path: Mutex<Option<PathBuf>>,
    /// Per-harness model discovery, loaded from disk at boot so the picker
    /// renders instantly instead of re-probing every agent on every launch.
    models_cache: Arc<Mutex<HashMap<HarnessId, CachedModels>>>,
    /// Where the model cache persists; `None` (tests, bare registries) skips
    /// disk reads/writes and `models()` always probes.
    models_cache_path: Mutex<Option<PathBuf>>,
}

impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            prefs: Mutex::new(HarnessPrefsFile::default()),
            prefs_path: Mutex::new(None),
            models_cache: Arc::new(Mutex::new(HashMap::new())),
            models_cache_path: Mutex::new(None),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<HarnessId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn order(&self) -> MutexGuard<'_, Vec<HarnessId>> {
        self.order.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn prefs(&self) -> MutexGuard<'_, HarnessPrefsFile> {
        self.prefs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load `harness-prefs.json` from the engine data dir and remember the
    /// path for writes. Corrupt/missing files fall back to the default set.
    pub fn load_prefs(&self, data_dir: &Path) {
        let path = data_dir.join("harness-prefs.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HarnessPrefsFile>(&text).ok())
            .unwrap_or_default();
        *self.prefs() = loaded;
        *self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
    }

    /// Load `model-cache.json` from the engine data dir (alongside the
    /// harness prefs) and remember the path for writes. Corrupt/missing
    /// files fall back to an empty cache — the first `models()` call probes.
    pub fn load_models_cache(&self, data_dir: &Path) {
        let path = data_dir.join("model-cache.json");
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ModelsCacheFile>(&text).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        *self
            .models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = loaded;
        *self
            .models_cache_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(path);
    }

    fn models_cache_entries(&self) -> MutexGuard<'_, HashMap<HarnessId, CachedModels>> {
        self.models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Best-effort atomic write (temp + rename, the prefs pattern).
    fn persist_models_cache(&self) {
        let Some(path) = self
            .models_cache_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let file = ModelsCacheFile {
            entries: self.models_cache_entries().clone(),
        };
        let json = match serde_json::to_string_pretty(&file) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "model-cache serialize failed");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %err, "model-cache save failed");
        }
    }

    /// The model list for `id` — the resolved harness's discovery, served
    /// stale-while-revalidate from the persistent cache. A fresh-enough
    /// entry renders instantly (no cold agent boot on picker open; the app's
    /// boot prefetch probes every offered harness in parallel, and opencode's
    /// Node startup alone measured ~13s), then re-probes in the background so
    /// the file stays warm for the next launch. A miss — or a probe failure
    /// with no usable fallback — probes now and propagates, exactly like the
    /// harness's own `models()`.
    pub async fn models(&self, id: HarnessId) -> Result<Vec<Model>, HarnessError> {
        let now = crate::now_ms();
        let ttl_ms = MODELS_CACHE_TTL.as_millis() as i64;
        let refresh_after_ms = MODELS_CACHE_REFRESH_AFTER.as_millis() as i64;

        // Stale-while-revalidate off the boot-loaded cache: a fresh-enough
        // entry serves instantly WITHOUT resolving the harness — the picker
        // renders before any agent boots (opencode's cold Node start measured
        // ~13s under the app's 4-way boot prefetch; user report: "model
        // loading is too slow"). Refresh in the background at most every
        // REFRESH_AFTER so the file stays warm without a probe per open.
        if let Some(cached) = self.models_cache_entries().get(&id).cloned() {
            let age = now - cached.discovered_at_ms;
            if age < ttl_ms {
                if age >= refresh_after_ms
                    && let Ok(harness) = self.resolve(id)
                {
                    let cache = Arc::clone(&self.models_cache);
                    let path = self
                        .models_cache_path
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    let id = id;
                    tokio::spawn(async move {
                        if let Ok(fresh) = harness.models().await
                            && !fresh.is_empty()
                        {
                            cache
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(id, CachedModels {
                                    discovered_at_ms: crate::now_ms(),
                                    models: fresh.clone(),
                                });
                            if let Some(path) = path {
                                let file = ModelsCacheFile {
                                    entries: cache
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .clone(),
                                };
                                let _ = serde_json::to_string_pretty(&file)
                                    .ok()
                                    .and_then(|json| std::fs::write(path, json).ok());
                            }
                        }
                    });
                }
                return Ok(cached.models);
            }
        }

        let harness = self.resolve(id)?;
        match harness.models().await {
            Ok(models) if !models.is_empty() => {
                self.models_cache_entries().insert(
                    id,
                    CachedModels {
                        discovered_at_ms: now,
                        models: models.clone(),
                    },
                );
                self.persist_models_cache();
                Ok(models)
            }
            // The wire advertised nothing (or the probe failed): fall back to
            // a stale cached list when one exists, then to the harness's own
            // answer (static catalog, or the propagated error for
            // wire-first harnesses).
            other => match self.models_cache_entries().get(&id).cloned() {
                Some(cached) if !cached.models.is_empty() => Ok(cached.models),
                _ => other,
            },
        }
    }

    /// The enabled set in effect (the default set until the user edits it).
    pub fn enabled_set(&self) -> Vec<HarnessId> {
        self.prefs().enabled.clone().unwrap_or_else(default_enabled)
    }

    /// Whether this device's CLI probe passes for `id` (no spawn, no resolve).
    fn installed_for(&self, id: HarnessId) -> bool {
        match self.slots().get(&id) {
            Some(Slot::Ready(harness)) => harness.installed(),
            Some(Slot::Lazy { installed, .. }) => installed(),
            None => false,
        }
    }

    /// Flip one harness's enablement and persist. Refuses unknown harnesses,
    /// enabling one whose CLI is missing (the settings gate, enforced where
    /// the state lives), and disabling the last enabled harness.
    pub fn set_enabled(&self, id: HarnessId, on: bool) -> Result<(), String> {
        if !self.slots().contains_key(&id) {
            return Err(format!("unknown harness {id:?}"));
        }
        if on && !self.installed_for(id) {
            return Err(format!("{id:?} CLI is not installed on this device"));
        }
        let mut set = self.enabled_set();
        match (on, set.contains(&id)) {
            (true, false) => set.push(id),
            (false, true) => {
                if set.len() == 1 {
                    return Err("cannot disable the last enabled harness".into());
                }
                set.retain(|h| *h != id);
            }
            _ => return Ok(()),
        }
        self.prefs().enabled = Some(set);
        self.persist_prefs();
        Ok(())
    }

    /// Best-effort atomic write (temp + rename, the ui-settings pattern).
    fn persist_prefs(&self) {
        let Some(path) = self
            .prefs_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let json = match serde_json::to_string_pretty(&*self.prefs()) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "harness-prefs serialize failed");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %err, "harness-prefs save failed");
        }
    }

    pub fn register(&self, harness: Arc<dyn Harness>) {
        let id = harness.id();
        if self.slots().insert(id, Slot::Ready(harness)).is_none() {
            self.order().push(id);
        }
    }

    /// Register a slot resolved on first `resolve` (the factory result is
    /// cached). `installed` is the CLI-presence probe run per `descriptors()`
    /// call; it must never spawn.
    pub fn register_lazy(
        &self,
        descriptor: HarnessDescriptor,
        installed: InstalledProbe,
        factory: Factory,
    ) {
        let id = descriptor.id;
        if self
            .slots()
            .insert(
                id,
                Slot::Lazy {
                    descriptor,
                    installed,
                    factory,
                },
            )
            .is_none()
        {
            self.order().push(id);
        }
    }

    pub fn resolve(&self, id: HarnessId) -> Result<Arc<dyn Harness>, HarnessError> {
        let mut slots = self.slots();
        match slots.get(&id) {
            Some(Slot::Ready(harness)) => Ok(harness.clone()),
            Some(Slot::Lazy { factory, .. }) => {
                let harness = factory()?;
                slots.insert(id, Slot::Ready(harness.clone()));
                Ok(harness)
            }
            None => Err(HarnessError::NotInstalled(format!("{id:?}"))),
        }
    }

    /// Catalog for `ListHarnesses` — never forces a lazy resolve.
    pub fn descriptors(&self) -> Vec<HarnessDescriptor> {
        let enabled = self.enabled_set();
        let slots = self.slots();
        self.order()
            .iter()
            .filter_map(|id| {
                let mut descriptor = match slots.get(id) {
                    Some(Slot::Ready(harness)) => describe(harness.as_ref()),
                    Some(Slot::Lazy {
                        descriptor,
                        installed,
                        ..
                    }) => HarnessDescriptor {
                        installed: installed(),
                        ..descriptor.clone()
                    },
                    None => return None,
                };
                descriptor.enabled = Some(enabled.contains(id));
                Some(descriptor)
            })
            .collect()
    }
}

/// The production registry: MockHarness (hidden from production pickers) plus a lazy
/// `claude-code` slot resolved through `zeron_harness` on first use (subprocess
/// discovery only happens when a run/model call actually needs it).
pub fn default_registry() -> HarnessRegistry {
    // Warm the login-shell PATH snapshot in the background so the first
    // claude/codex resolve doesn't pay the shell-startup latency inline.
    zeron_harness::shell_env::prewarm();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: vec![
            AgentEvent::TextDelta {
                text: "## Streaming pipeline\n\nEvery turn flows through the same path:\n\n".into(),
            },
            AgentEvent::TextDelta {
                text: "1. **Doc command** — the composer queues a durable `run` entry\n2. **Host executor** — the chat's host device marks it processed, then dispatches\n3. **Fold** — events fold into parts and diff into the Loro doc every 120ms\n\n".into(),
            },
            AgentEvent::ToolCall {
                id: "mock-tool-1".into(),
                call: zeron_proto::ToolCall::Exec {
                    command: "cargo test --workspace".into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-1".into(),
                is_error: false,
                output: None,
                diff: None,
            },
            AgentEvent::ToolCall {
                id: "mock-tool-2".into(),
                call: zeron_proto::ToolCall::Exec {
                    command: "git log -5 --oneline --decorate && git merge-base HEAD origin/main"
                        .into(),
                },
            },
            AgentEvent::ToolResult {
                id: "mock-tool-2".into(),
                is_error: false,
                output: None,
                diff: None,
            },
            AgentEvent::TextDelta {
                text: "The `SegmentWriter` appends into `LoroText` so the oplog stays RLE-merged:\n\n```rust\nfolded = fold_event_into_parts(&folded, &event);\nwriter.sync(&folded)?; // 120ms coalesced commits\n```\n\nSynced to every device through the session room. *Mock harness reporting in.*".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    }));
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::ClaudeCode,
            name: "Claude Code".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            // Must mirror AcpHarness::claude()'s spec exactly — the
            // descriptor-stability rule (see the codex test below).
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::claude().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::claude()) as Arc<dyn Harness>)),
    );
    // Codex, same lazy pattern: the static descriptor mirrors AcpHarness::codex()
    // exactly (`describe()` after the first resolve must not change the
    // catalog entry) — "Codex" per the original HARNESS_LABEL, StepBoundary
    // steering via native `turn/steer`, and the unified reasoning ladder from
    // zeron_harness::codex::catalog. CLI discovery only happens when a
    // run/model call actually resolves the slot.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Codex,
            name: "Codex".into(),
            supports_steering: true,
            steering_mode: SteeringMode::StepBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
                ReasoningLevel::Ultra,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::codex().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::codex()) as Arc<dyn Harness>)),
    );
    // Cursor Agent over ACP (`cursor-agent acp`), same lazy pattern: the
    // static descriptor mirrors AcpHarness::cursor() exactly. No steering
    // extension (turn boundaries) and no effort ladder — Cursor bakes effort
    // into the model id's bracket suffix instead of a `thought_level` option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Cursor,
            name: "Cursor".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::cursor().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::cursor()) as Arc<dyn Harness>)),
    );
    // Grok Build over ACP, same lazy pattern: the static descriptor mirrors
    // AcpHarness::grok() exactly. No `_session/steering` extension yet, so
    // steers deliver at turn boundaries; the effort ladder applies per
    // session via the `thought_level` config option.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Grok,
            name: "Grok".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::grok().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::grok()) as Arc<dyn Harness>)),
    );
    // Hermes Agent over ACP (`hermes acp`), same lazy pattern: the static
    // descriptor mirrors AcpHarness::hermes() exactly. No steering extension
    // (turn boundaries) and no effort ladder — Hermes exposes no effort
    // config over ACP today (hybrid reasoning is model-internal).
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Hermes,
            name: "Hermes".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::hermes().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::hermes()) as Arc<dyn Harness>)),
    );
    // opencode over its native ACP server (`opencode acp`), same lazy
    // pattern: the static descriptor mirrors AcpHarness::opencode() exactly.
    // No steering extension and no thought_level, so steers deliver at turn
    // boundaries with an empty reasoning ladder.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::OpenCode,
            name: "OpenCode".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: Vec::new(),
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::opencode().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::opencode()) as Arc<dyn Harness>)),
    );
    // pi over ACP (community `pi-acp` adapter), same lazy pattern: the static
    // descriptor mirrors AcpHarness::pi() exactly — turn-boundary steering,
    // pi's thinking ladder minus its "off" tier.
    registry.register_lazy(
        HarnessDescriptor {
            id: HarnessId::Pi,
            name: "Pi".into(),
            supports_steering: true,
            steering_mode: SteeringMode::TurnBoundary,
            reasoning_levels: vec![
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ],
            installed: true,
            enabled: None,
        },
        Box::new(|| zeron_harness::AcpHarness::pi().installed()),
        Box::new(|| Ok(Arc::new(zeron_harness::AcpHarness::pi()) as Arc<dyn Harness>)),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_slot_lists_without_resolving() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let registry = HarnessRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        registry.register_lazy(
            HarnessDescriptor {
                id: HarnessId::Mock,
                name: "Lazy Mock".into(),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
            },
            Box::new(|| false),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::NotInstalled("nope".into()))
            }),
        );
        let listed = registry.descriptors();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Lazy Mock");
        // The listing runs the probe, not the stored placeholder.
        assert!(!listed[0].installed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "listing must not force a resolve"
        );
        assert!(registry.resolve(HarnessId::Mock).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_registry_lists_mock_claude_codex_and_grok_slots() {
        let registry = default_registry();
        let ids: Vec<HarnessId> = registry.descriptors().iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                HarnessId::Mock,
                HarnessId::ClaudeCode,
                HarnessId::Codex,
                HarnessId::Cursor,
                HarnessId::Grok,
                HarnessId::Hermes,
                HarnessId::OpenCode,
                HarnessId::Pi
            ]
        );
        assert!(registry.resolve(HarnessId::Mock).is_ok());
        assert!(registry.resolve(HarnessId::ClaudeCode).is_ok());
        // A codex-configured chat resolves the right harness (construction is
        // cheap; CLI discovery is deferred to models()/run()).
        let codex = registry.resolve(HarnessId::Codex).unwrap();
        assert_eq!(codex.id(), HarnessId::Codex);
        // Grok resolves through the shared ACP harness; its descriptor must
        // mirror the resolved harness (descriptor-stability rule).
        let grok = registry.resolve(HarnessId::Grok).unwrap();
        assert_eq!(grok.id(), HarnessId::Grok);
        assert_eq!(grok.display_name(), "Grok");
        assert_eq!(grok.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            grok.reasoning_levels(),
            &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High
            ]
        );
        // Cursor, Hermes and Pi mirror their specs the same way.
        let cursor = registry.resolve(HarnessId::Cursor).unwrap();
        assert_eq!(cursor.id(), HarnessId::Cursor);
        assert_eq!(cursor.display_name(), "Cursor");
        assert_eq!(cursor.steering_mode(), SteeringMode::TurnBoundary);
        assert!(cursor.reasoning_levels().is_empty());
        let hermes = registry.resolve(HarnessId::Hermes).unwrap();
        assert_eq!(hermes.id(), HarnessId::Hermes);
        assert_eq!(hermes.display_name(), "Hermes");
        assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
        assert!(hermes.reasoning_levels().is_empty());
        let opencode = registry.resolve(HarnessId::OpenCode).unwrap();
        assert_eq!(opencode.id(), HarnessId::OpenCode);
        assert_eq!(opencode.display_name(), "OpenCode");
        assert_eq!(opencode.steering_mode(), SteeringMode::TurnBoundary);
        assert!(opencode.reasoning_levels().is_empty());
        let pi = registry.resolve(HarnessId::Pi).unwrap();
        assert_eq!(pi.id(), HarnessId::Pi);
        assert_eq!(pi.display_name(), "Pi");
        assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
        assert_eq!(
            pi.reasoning_levels(),
            &[
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max
            ]
        );
    }

    /// Catalogs serialized by engines that predate the `installed`/`enabled`
    /// fields must keep deserializing — installed, and enabled per the
    /// default-set fallback (Claude Code yes, Grok no).
    #[test]
    fn descriptor_without_new_fields_parses_with_fallbacks() {
        let parse = |id: &str| -> HarnessDescriptor {
            serde_json::from_str(&format!(
                r#"{{
                    "id": "{id}",
                    "name": "x",
                    "supportsSteering": true,
                    "steeringMode": "step-boundary",
                    "reasoningLevels": []
                }}"#
            ))
            .unwrap()
        };
        let claude = parse("claude-code");
        assert!(claude.installed);
        assert_eq!(claude.enabled, None);
        assert!(descriptor_enabled(&claude));
        assert!(!descriptor_enabled(&parse("grok")));
    }

    /// A registry slot for the tests below: installed probe fixed, factory
    /// never expected to run.
    fn test_slot(registry: &HarnessRegistry, id: HarnessId, installed: bool) {
        registry.register_lazy(
            HarnessDescriptor {
                id,
                name: format!("{id:?}"),
                supports_steering: true,
                steering_mode: SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed: true,
                enabled: None,
            },
            Box::new(move || installed),
            Box::new(|| Err(HarnessError::NotInstalled("test slot".into()))),
        );
    }

    /// `descriptors()` stamps the per-device enabled flag; `set_enabled`
    /// guards the gate (no enabling missing CLIs, no disabling the last one)
    /// and persists across a reload.
    #[test]
    fn enablement_stamps_guards_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        test_slot(&registry, HarnessId::ClaudeCode, true);
        test_slot(&registry, HarnessId::Codex, true);
        test_slot(&registry, HarnessId::Grok, true);
        test_slot(&registry, HarnessId::Hermes, false);

        // Default set stamped: Claude Code + Codex on, the rest off.
        let flags: Vec<(HarnessId, Option<bool>)> = registry
            .descriptors()
            .into_iter()
            .map(|d| (d.id, d.enabled))
            .collect();
        assert_eq!(
            flags,
            vec![
                (HarnessId::ClaudeCode, Some(true)),
                (HarnessId::Codex, Some(true)),
                (HarnessId::Grok, Some(false)),
                (HarnessId::Hermes, Some(false)),
            ]
        );

        // The gate: a missing CLI can't be enabled; unknown ids refuse.
        assert!(registry.set_enabled(HarnessId::Hermes, true).is_err());
        assert!(registry.set_enabled(HarnessId::Pi, true).is_err());
        // Installed CLIs toggle both ways; no-op flips are fine.
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Grok, true).unwrap();
        registry.set_enabled(HarnessId::Codex, false).unwrap();
        registry.set_enabled(HarnessId::ClaudeCode, false).unwrap();
        // Grok is the last one standing — refusing keeps the composer usable.
        assert!(registry.set_enabled(HarnessId::Grok, false).is_err());
        assert_eq!(registry.enabled_set(), vec![HarnessId::Grok]);

        // A fresh registry over the same data dir reads the persisted set.
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        assert_eq!(reloaded.enabled_set(), vec![HarnessId::Grok]);
    }

    /// Discovered models persist to `model-cache.json` and serve a fresh
    /// registry from disk — the picker never re-probes an agent across app
    /// launches (opencode's cold Node boot measured ~13s under the app's
    /// 4-way boot prefetch; user report: "model loading is too slow").
    #[tokio::test]
    async fn models_cache_persists_and_reloads_from_disk() {
        use zeron_harness::mock::MockHarness;
        let dir = tempfile::tempdir().unwrap();

        let registry = HarnessRegistry::new();
        registry.load_prefs(dir.path());
        registry.load_models_cache(dir.path());
        registry.register(Arc::new(MockHarness { script: Vec::new() }));

        // A cache miss probes the harness and writes through.
        let models = registry.models(HarnessId::Mock).await.unwrap();
        assert!(!models.is_empty(), "{models:?}");
        assert!(
            dir.path().join("model-cache.json").exists(),
            "a successful probe must persist the list"
        );

        // A fresh registry over the same data dir serves the cached list
        // without re-probing (the harness was never registered here).
        let reloaded = HarnessRegistry::new();
        reloaded.load_prefs(dir.path());
        reloaded.load_models_cache(dir.path());
        let cached = reloaded.models(HarnessId::Mock).await;
        let ids: Vec<String> = cached
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["mock-1", "mock-fable-5"]);
    }

    /// A fresh cache entry serves instantly (age under the refresh window),
    /// while an empty wire result falls back to a stale cached list instead
    /// of returning nothing.
    #[tokio::test]
    async fn models_cache_serves_fresh_and_falls_back_on_empty() {
        use zeron_harness::mock::MockHarness;
        let dir = tempfile::tempdir().unwrap();

        let registry = HarnessRegistry::new();
        registry.load_models_cache(dir.path());
        registry.register(Arc::new(MockHarness { script: Vec::new() }));
        let models = registry.models(HarnessId::Mock).await.unwrap();
        assert_eq!(models.len(), 2);

        // Fresh path: second call returns from the in-memory cache (no
        // re-probe observable, but the entry's age stays under the window).
        let again = registry.models(HarnessId::Mock).await.unwrap();
        assert_eq!(again.len(), 2);
        let entry = registry
            .models_cache_entries()
            .get(&HarnessId::Mock)
            .cloned()
            .unwrap();
        assert!(crate::now_ms() - entry.discovered_at_ms < 60_000);

        // Fallback: a wire-first harness whose probe advertises nothing
        // keeps the previously-cached list rather than emptying the picker.
        registry.models_cache_entries().insert(
            HarnessId::OpenCode,
            CachedModels {
                discovered_at_ms: crate::now_ms(),
                models: vec![Model {
                    id: "opencode/smol".into(),
                    label: "Smol".into(),
                    description: None,
                    reasoning_levels: vec![],
                    options: vec![],
                }],
            },
        );
        // An empty wire answer (no catalog, no list) falls back to the cache.
        let fallback = {
            let harness = zeron_harness::AcpHarness::opencode()
                .with_executable("/nonexistent/never-an-opencode-acp");
            // resolve via a lazy slot whose factory serves the harness
            // directly, so the fallback path is what decides.
            let reg = HarnessRegistry::new();
            reg.register(Arc::new(harness) as Arc<dyn Harness>);
            // Seed the same cache entry the parent holds.
            *reg.models_cache_entries() = registry.models_cache_entries().clone();
            reg.models(HarnessId::OpenCode).await
        };
        assert_eq!(
            fallback.unwrap()[0].id,
            "opencode/smol",
            "a stale cached list must outlive an empty wire probe"
        );
    }

    /// The Codex lazy descriptor must be indistinguishable from `describe()`
    /// after the first resolve — otherwise the catalog entry silently changes
    /// the moment the harness is used (name/ladder flip in the picker rail).
    /// (KNOWN GAP, predates this slot: the claude-code descriptor advertises
    /// `[Ultrathink]` while the resolved adapter reports `[Low..Max]` — left
    /// as-is here; flagged for its own pass.)
    #[test]
    fn codex_lazy_descriptor_matches_resolved_harness() {
        let registry = default_registry();
        let before = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        registry.resolve(HarnessId::Codex).unwrap();
        let after = registry
            .descriptors()
            .into_iter()
            .find(|d| d.id == HarnessId::Codex)
            .unwrap();
        assert_eq!(before.name, after.name);
        assert_eq!(before.supports_steering, after.supports_steering);
        assert_eq!(before.steering_mode, after.steering_mode);
        assert_eq!(before.reasoning_levels, after.reasoning_levels);
    }
}
