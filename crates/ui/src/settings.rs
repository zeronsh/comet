//! UI settings persisted to a small JSON file in the data dir — pane widths and
//! collapse flags (zeron persisted the same set in localStorage).
//!
//! Loaded once at boot and then owned by [`SettingsStore`], the only production
//! writer. Frequent geometry changes are debounced; durable choices flush
//! immediately through that same writer. Corrupt or missing files fall back to
//! defaults, and loaded values are clamped so a hand-edited file can't wedge the
//! layout.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{App, Global, Task};
use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod appearance;
pub mod archived;
pub mod composer;
pub mod devices;
pub mod harnesses;
pub mod notifications;
pub mod shortcuts;
pub mod widgets;

/// Sidebar drag-resize bounds (px).
pub const SIDEBAR_MIN: f32 = 208.0;
pub const SIDEBAR_MAX: f32 = 400.0;
pub const SIDEBAR_DEFAULT: f32 = 256.0;

/// Right ("Changes") pane drag-resize floor and default (px). Its runtime
/// maximum is the window space remaining after the left sidebar and the
/// conversation's [`CHAT_PANEL_MIN`] reservation.
pub const RIGHT_PANE_MIN: f32 = 360.0;
pub const RIGHT_PANE_DEFAULT: f32 = 520.0;
/// Minimum width retained for the conversation when the right pane is open.
pub const CHAT_PANEL_MIN: f32 = 300.0;

/// Terminal panel height bounds: 160px … 55% of the viewport (§1.10). The
/// viewport-relative cap applies at runtime; the absolute cap here only heals
/// hand-edited files.
pub const TERMINAL_MIN_HEIGHT: f32 = 160.0;
pub const TERMINAL_MAX_VH: f32 = 0.55;
pub const TERMINAL_ABS_MAX_HEIGHT: f32 = 2000.0;
pub const TERMINAL_DEFAULT_HEIGHT: f32 = 280.0;

/// Debounce for settings writes after a drag/toggle.
pub const SAVE_DEBOUNCE_MS: u64 = 400;

const FILE_NAME: &str = "ui-settings.json";

/// Whether a settings mutation should wait for the normal coalescing window or
/// reach disk before returning to the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePolicy {
    Debounced,
    Immediate,
}

/// The sole in-process owner and writer of `ui-settings.json`.
///
/// Mutations land in `current` before any timer starts. Replacing a pending
/// task cancels its stale snapshot, and immediate mutations cancel the timer
/// before flushing synchronously. The file is tiny, so keeping the atomic
/// write on GPUI's foreground executor is preferable to allowing concurrent
/// background writers that can finish out of order.
pub struct SettingsStore {
    current: UiSettings,
    data_dir: PathBuf,
    revision: u64,
    saved_revision: u64,
    save_task: Option<Task<()>>,
}

impl Global for SettingsStore {}

impl SettingsStore {
    fn snapshot(&self) -> (UiSettings, u64) {
        (self.current.clone(), self.revision)
    }

    fn mark_saved(&mut self, revision: u64) -> bool {
        self.saved_revision = self.saved_revision.max(revision);
        self.saved_revision == self.revision
    }
}

/// Install the settings loaded at boot as the process-wide source of truth.
pub fn init(settings: UiSettings, data_dir: impl Into<PathBuf>, cx: &mut App) {
    cx.set_global(SettingsStore {
        current: settings,
        data_dir: data_dir.into(),
        revision: 0,
        saved_revision: 0,
        save_task: None,
    });
}

/// Latest settings, including mutations that are still inside the debounce
/// window and therefore may not have reached disk yet.
pub fn current(cx: &App) -> UiSettings {
    cx.try_global::<SettingsStore>()
        .map(|store| store.current.clone())
        .unwrap_or_default()
}

/// Mutate the central settings value and schedule its single writer.
pub fn update(policy: SavePolicy, cx: &mut App, mutate: impl FnOnce(&mut UiSettings)) -> bool {
    let Some(store) = cx.try_global::<SettingsStore>() else {
        return false;
    };
    let before = store.current.clone();
    let store = cx.global_mut::<SettingsStore>();
    mutate(&mut store.current);
    store.current = store.current.clone().clamped();
    if store.current == before {
        return false;
    }
    store.revision = store.revision.wrapping_add(1);
    schedule(policy, cx);
    true
}

/// Replace the central value from a view that owns a working copy, such as the
/// shell's pane geometry state.
pub fn replace(settings: UiSettings, policy: SavePolicy, cx: &mut App) -> bool {
    update(policy, cx, |current| *current = settings)
}

fn schedule(policy: SavePolicy, cx: &mut App) {
    let old_task = cx.global_mut::<SettingsStore>().save_task.take();
    drop(old_task);

    match policy {
        SavePolicy::Immediate => flush(cx),
        SavePolicy::Debounced => {
            let task = cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                    .await;
                cx.update(flush_latest);
            });
            cx.global_mut::<SettingsStore>().save_task = Some(task);
        }
    }
}

/// Persist the latest revision. Safe to call at shutdown; no task is spawned.
pub fn flush(cx: &mut App) {
    if !cx.has_global::<SettingsStore>() {
        return;
    }
    let pending = cx.global_mut::<SettingsStore>().save_task.take();
    drop(pending);
    flush_latest(cx);
}

fn flush_latest(cx: &mut App) {
    let Some(store) = cx.try_global::<SettingsStore>() else {
        return;
    };
    if store.saved_revision == store.revision {
        return;
    }
    let (settings, revision) = store.snapshot();
    let data_dir = store.data_dir.clone();
    match settings.save(&data_dir) {
        Ok(()) => {
            let current = cx.global_mut::<SettingsStore>().mark_saved(revision);
            debug_assert!(current, "foreground settings write cannot be overtaken");
        }
        Err(err) => tracing::warn!(error = %err, revision, "failed to persist ui settings"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// Legacy: the grouped-by-project toggle predates spaces (which group by
    /// folder inherently). Kept for file compatibility; no longer read.
    pub sidebar_grouped: bool,
    /// The last selected space — restored on boot when the row still exists;
    /// also the new-tab default when the sidebar filter is "All".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_space_id: Option<String>,
    /// Open session tabs in visual order (drag-reorder edits in place).
    /// Device-local: a tab is a local viewport onto the synced session list —
    /// closing one never archives the session. Ids of archived/deleted chats
    /// are pruned against the doc ([`Shell::sync_open_tabs`]). `None` = file
    /// written by a pre-tabs build; seeded once from the last space's sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_tabs: Option<Vec<String>>,
    /// Sidebar session filter: a space id, or `None` for "All spaces".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_filter: Option<String>,
    /// Legacy: per-space tab order, from when tabs were the selected space's
    /// non-archived sessions. Kept for file compatibility; no longer read.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tab_order: std::collections::HashMap<String, Vec<String>>,
    /// Legacy: manual sidebar space order, from when spaces were a sidebar
    /// list. Kept for file compatibility; no longer read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_order: Vec<String>,
    /// Session notification chimes (done / awaiting-input). `ZERON_DISABLE_SOUND`
    /// overrides.
    pub sound_enabled: bool,
    /// Desktop banner notifications on the same transitions.
    /// `ZERON_DISABLE_NOTIFICATIONS` overrides.
    pub notifications_enabled: bool,
    /// Suppress the banner while a Zeron window is focused (the chime covers
    /// the foreground case).
    pub notifications_background_only: bool,
    pub right_pane_width: f32,
    /// Legacy: panel *open* flags are session-scoped in-memory state now
    /// (`shell::SessionPanels`, zeron `sessionPanels` parity). Kept for file
    /// compatibility; no longer read or written by the shell.
    pub right_pane_open: bool,
    pub terminal_height: f32,
    /// Legacy — see [`Self::right_pane_open`].
    pub terminal_open: bool,
    /// Customizable shortcut combos (feature-inventory §1.4).
    pub keymap: KeymapConfig,
    /// Light/dark preference. Defaults to following the OS.
    pub appearance: crate::appearance::AppearanceMode,
    /// Interface and conversational-prose family. Device-local by design.
    pub ui_font_family: crate::typography::UiFontFamily,
    /// Base size for interface and conversational prose. Code-related surfaces
    /// retain their fixed metrics.
    pub ui_font_size: crate::typography::UiFontSize,
    /// Changes pane: side-by-side diffs instead of the unified stack.
    pub diff_split: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            sidebar_grouped: false,
            last_space_id: None,
            open_tabs: None,
            space_filter: None,
            tab_order: std::collections::HashMap::new(),
            space_order: Vec::new(),
            sound_enabled: true,
            notifications_enabled: true,
            notifications_background_only: true,
            right_pane_width: RIGHT_PANE_DEFAULT,
            right_pane_open: false,
            terminal_height: TERMINAL_DEFAULT_HEIGHT,
            terminal_open: false,
            keymap: KeymapConfig::default(),
            appearance: crate::appearance::AppearanceMode::default(),
            ui_font_family: crate::typography::UiFontFamily::default(),
            ui_font_size: crate::typography::UiFontSize::default(),
            diff_split: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Keymap (customizable shortcuts, §1.4)
// ---------------------------------------------------------------------------

/// The rebindable app shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutId {
    ToggleSidebar,
    ToggleChanges,
    ToggleTerminal,
    NewSession,
}

impl ShortcutId {
    pub const ALL: [ShortcutId; 4] = [
        ShortcutId::ToggleSidebar,
        ShortcutId::ToggleChanges,
        ShortcutId::ToggleTerminal,
        ShortcutId::NewSession,
    ];

    /// Row label (zeron lib/shortcuts.ts `SHORTCUT_DEFINITIONS`, verbatim).
    pub fn label(self) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "Toggle left sidebar",
            ShortcutId::ToggleChanges => "Toggle right sidebar",
            ShortcutId::ToggleTerminal => "Toggle terminal",
            ShortcutId::NewSession => "New session",
        }
    }

    pub fn default_combo(self) -> &'static str {
        match self {
            ShortcutId::ToggleSidebar => "mod-s",
            ShortcutId::ToggleChanges => "mod-b",
            ShortcutId::ToggleTerminal => "mod-j",
            ShortcutId::NewSession => "mod-n",
        }
    }
}

/// Persisted shortcut combos. Stored platform-neutral ("mod-s"); translated to
/// "cmd-s"/"ctrl-s" at bind time by [`platform_combo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub toggle_sidebar: String,
    pub toggle_changes: String,
    pub toggle_terminal: String,
    pub new_session: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            toggle_sidebar: ShortcutId::ToggleSidebar.default_combo().into(),
            toggle_changes: ShortcutId::ToggleChanges.default_combo().into(),
            toggle_terminal: ShortcutId::ToggleTerminal.default_combo().into(),
            new_session: ShortcutId::NewSession.default_combo().into(),
        }
    }
}

impl KeymapConfig {
    pub fn get(&self, id: ShortcutId) -> &str {
        match id {
            ShortcutId::ToggleSidebar => &self.toggle_sidebar,
            ShortcutId::ToggleChanges => &self.toggle_changes,
            ShortcutId::ToggleTerminal => &self.toggle_terminal,
            ShortcutId::NewSession => &self.new_session,
        }
    }

    pub fn set(&mut self, id: ShortcutId, combo: String) {
        match id {
            ShortcutId::ToggleSidebar => self.toggle_sidebar = combo,
            ShortcutId::ToggleChanges => self.toggle_changes = combo,
            ShortcutId::ToggleTerminal => self.toggle_terminal = combo,
            ShortcutId::NewSession => self.new_session = combo,
        }
    }

    pub fn reset(&mut self, id: ShortcutId) {
        self.set(id, id.default_combo().to_string());
    }
}

/// Build a combo string from a recorded keystroke. The primary modifier
/// (cmd on macOS, ctrl elsewhere — either recorded key maps in) becomes "mod";
/// bare modifier presses record nothing.
pub fn combo_from_keystroke(
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    key: &str,
) -> Option<String> {
    let key = key.trim().to_lowercase();
    if key.is_empty()
        || matches!(
            key.as_str(),
            "ctrl" | "control" | "alt" | "shift" | "cmd" | "platform" | "fn"
        )
    {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if ctrl || cmd {
        parts.push("mod");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    parts.push(&key);
    Some(parts.join("-"))
}

/// Shortcut ids whose combos collide with another shortcut (conflict detection).
pub fn conflicted_shortcuts(keymap: &KeymapConfig) -> Vec<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .filter(|&id| {
            let combo = keymap.get(id);
            !combo.is_empty()
                && ShortcutId::ALL
                    .into_iter()
                    .any(|other| other != id && keymap.get(other) == combo)
        })
        .collect()
}

/// Translate a stored combo into a bindable keystroke for this platform.
pub fn platform_combo(combo: &str) -> String {
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    combo
        .split('-')
        .map(|part| if part == "mod" { primary } else { part })
        .collect::<Vec<_>>()
        .join("-")
}

/// Human-readable combo for the shortcuts table ("mod-s" → "Cmd+S"/"Ctrl+S").
pub fn display_combo(combo: &str) -> String {
    combo
        .split('-')
        .map(|part| match part {
            "mod" => {
                if cfg!(target_os = "macos") {
                    "Cmd".to_string()
                } else {
                    "Ctrl".to_string()
                }
            }
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl UiSettings {
    /// Clamp widths into their legal ranges (also heals NaN to defaults).
    pub fn clamped(mut self) -> Self {
        self.sidebar_width = clamp_or(
            self.sidebar_width,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            SIDEBAR_DEFAULT,
        );
        // The right pane has no persisted upper bound: its live drag clamps
        // against the current window, which is unavailable while loading.
        self.right_pane_width = min_or(self.right_pane_width, RIGHT_PANE_MIN, RIGHT_PANE_DEFAULT);
        self.terminal_height = clamp_or(
            self.terminal_height,
            TERMINAL_MIN_HEIGHT,
            TERMINAL_ABS_MAX_HEIGHT,
            TERMINAL_DEFAULT_HEIGHT,
        );
        self.ui_font_size = self.ui_font_size.normalized();
        self
    }

    /// Load from `{data_dir}/ui-settings.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<UiSettings>(&text) {
                Ok(settings) => settings.clamped(),
                Err(err) => {
                    tracing::warn!(error = %err, "ui-settings corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

fn min_or(value: f32, min: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.max(min)
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = UiSettings {
            sidebar_width: 300.0,
            sidebar_collapsed: true,
            sidebar_grouped: true,
            last_space_id: Some("space-1".into()),
            open_tabs: Some(vec!["b".to_string(), "a".to_string()]),
            space_filter: Some("space-1".into()),
            tab_order: std::collections::HashMap::from([(
                "space-1".to_string(),
                vec!["b".to_string(), "a".to_string()],
            )]),
            space_order: vec!["space-2".to_string(), "space-1".to_string()],
            sound_enabled: false,
            notifications_enabled: false,
            notifications_background_only: false,
            right_pane_width: 700.0,
            right_pane_open: true,
            terminal_height: 320.0,
            terminal_open: true,
            keymap: KeymapConfig {
                toggle_sidebar: "mod-shift-s".into(),
                ..KeymapConfig::default()
            },
            appearance: crate::appearance::AppearanceMode::Light,
            ui_font_family: crate::typography::UiFontFamily::Installed("Arial".into()),
            ui_font_size: crate::typography::UiFontSize::ALL[5],
            diff_split: true,
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(UiSettings::load(dir.path()), settings);
    }

    #[test]
    fn stale_revision_cannot_be_considered_the_latest_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SettingsStore {
            current: UiSettings::default(),
            data_dir: dir.path().to_path_buf(),
            revision: 0,
            saved_revision: 0,
            save_task: None,
        };

        store.current.sidebar_width = 300.0;
        store.revision += 1;
        let (stale, stale_revision) = store.snapshot();

        store.current.ui_font_family = crate::typography::UiFontFamily::Installed("Arial".into());
        store.revision += 1;
        stale.save(dir.path()).unwrap();
        assert!(!store.mark_saved(stale_revision));

        let (latest, latest_revision) = store.snapshot();
        latest.save(dir.path()).unwrap();
        assert!(store.mark_saved(latest_revision));
        let reloaded = UiSettings::load(dir.path());
        assert_eq!(reloaded.sidebar_width, 300.0);
        assert_eq!(
            reloaded.ui_font_family,
            crate::typography::UiFontFamily::Installed("Arial".into())
        );
    }

    /// A settings file written before light mode existed has no `appearance`
    /// key; it must load as "follow the OS" rather than failing the whole parse
    /// and resetting every other preference to defaults.
    #[test]
    fn settings_without_appearance_default_to_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.appearance, crate::appearance::AppearanceMode::System);
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled, "other keys still parse");
        assert!(
            loaded.notifications_enabled,
            "pre-banner files default banners on"
        );
        assert!(
            loaded.notifications_background_only,
            "pre-banner files default background-only on"
        );
    }

    #[test]
    fn settings_without_ui_font_default_to_geist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(
            loaded.ui_font_family,
            crate::typography::UiFontFamily::Geist
        );
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(!loaded.sound_enabled);
        assert_eq!(
            loaded.ui_font_size,
            crate::typography::UiFontSize::default()
        );
    }

    #[test]
    fn unsupported_ui_font_size_snaps_to_the_nearest_choice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"uiFontSize": 19, "soundEnabled": false}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.ui_font_size.pixels(), 18.0);
        assert!(!loaded.sound_enabled);
    }

    #[test]
    fn unknown_ui_font_falls_back_without_resetting_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "uiFontFamily": "futureSans"}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(
            loaded.ui_font_family,
            crate::typography::UiFontFamily::Geist
        );
        assert_eq!(loaded.sidebar_width, 300.0);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
        std::fs::write(UiSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 10000, "rightPaneWidth": 1}"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, SIDEBAR_MAX);
        assert_eq!(loaded.right_pane_width, RIGHT_PANE_MIN);
    }

    #[test]
    fn large_right_pane_width_is_preserved() {
        let loaded = UiSettings {
            right_pane_width: 2400.0,
            ..Default::default()
        }
        .clamped();
        assert_eq!(loaded.right_pane_width, 2400.0);
    }

    #[test]
    fn nan_heals_to_default() {
        let healed = UiSettings {
            sidebar_width: f32::NAN,
            ..Default::default()
        }
        .clamped();
        assert_eq!(healed.sidebar_width, SIDEBAR_DEFAULT);
    }

    #[test]
    fn defaults_match_zeron() {
        let d = UiSettings::default();
        assert_eq!(d.sidebar_width, 256.0);
        assert_eq!(d.right_pane_width, 520.0);
        assert_eq!(d.terminal_height, 280.0);
        assert!(!d.sidebar_collapsed && !d.right_pane_open && !d.terminal_open);
    }

    #[test]
    fn keymap_defaults_and_reset() {
        let mut keymap = KeymapConfig::default();
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
        assert_eq!(keymap.get(ShortcutId::ToggleChanges), "mod-b");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-j");
        keymap.set(ShortcutId::ToggleSidebar, "mod-shift-x".into());
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        keymap.reset(ShortcutId::ToggleSidebar);
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-s");
    }

    #[test]
    fn combo_recording() {
        // Primary modifier (ctrl or cmd) normalizes to "mod".
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(false, false, false, true, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(true, true, true, false, "K"),
            Some("mod-alt-shift-k".into())
        );
        // Plain keys record without modifiers (Esc is filtered by the caller).
        assert_eq!(
            combo_from_keystroke(false, false, false, false, "f5"),
            Some("f5".into())
        );
        // Bare modifier presses record nothing.
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "ctrl"),
            None
        );
        assert_eq!(
            combo_from_keystroke(false, false, true, false, "shift"),
            None
        );
        assert_eq!(combo_from_keystroke(false, false, false, false, ""), None);
    }

    #[test]
    fn conflict_detection() {
        let mut keymap = KeymapConfig::default();
        assert!(conflicted_shortcuts(&keymap).is_empty());
        keymap.set(ShortcutId::ToggleChanges, "mod-s".into());
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::ToggleSidebar));
        assert!(conflicts.contains(&ShortcutId::ToggleChanges));
        assert!(!conflicts.contains(&ShortcutId::ToggleTerminal));
        keymap.reset(ShortcutId::ToggleChanges);
        assert!(conflicted_shortcuts(&keymap).is_empty());
    }

    #[test]
    fn combo_translation() {
        let primary = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        assert_eq!(platform_combo("mod-s"), format!("{primary}-s"));
        assert_eq!(platform_combo("alt-f4"), "alt-f4");
        let display_primary = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        assert_eq!(
            display_combo("mod-shift-s"),
            format!("{display_primary}+Shift+S")
        );
        assert_eq!(display_combo("f5"), "F5");
    }

    #[test]
    fn keymap_survives_old_settings_files() {
        // Files written before the keymap existed load with defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"sidebarWidth": 300}"#).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.keymap, KeymapConfig::default());
        assert!(!loaded.sidebar_grouped);
    }

    #[test]
    fn terminal_height_clamps_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 5}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_MIN_HEIGHT
        );
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 99999}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_ABS_MAX_HEIGHT
        );
    }
}
