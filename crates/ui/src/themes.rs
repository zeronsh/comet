//! Theme registry: built-in theme families as [`ThemeColors`] data.
//!
//! Phase 0 ships only the two stock appearances (identical to the pre-theme
//! constructors). Later phases add palette families (Catppuccin, Tokyo Night,
//! Rosé Pine, …) as new entries; a family contributes one entry per appearance.
//!
//! Selection model mirrors Zed's: the user picks an [`AppearanceMode`]
//! (system/light/dark) *and* which theme to use for each appearance. Unknown or
//! missing ids fall back to the stock palette so a settings file written by a
//! newer build never breaks boot.

use crate::theme::{Appearance, ThemeColors};

pub const STOCK_DARK: &str = "zeron-dark";
pub const STOCK_LIGHT: &str = "zeron-light";

/// One selectable theme: identity plus palette data for exactly one appearance.
#[derive(Debug, Clone)]
pub struct ThemeEntry {
    /// Stable id persisted in `ui-settings.json`.
    pub id: &'static str,
    /// Picker label.
    pub name: &'static str,
    pub appearance: Appearance,
    pub colors: ThemeColors,
}

/// All built-in themes, one entry per (family, appearance).
pub fn built_ins() -> &'static [ThemeEntry] {
    static BUILT_INS: std::sync::OnceLock<Vec<ThemeEntry>> = std::sync::OnceLock::new();
    BUILT_INS.get_or_init(|| {
        vec![
            ThemeEntry {
                id: STOCK_DARK,
                name: "Zeron Dark",
                appearance: Appearance::Dark,
                colors: ThemeColors::stock_dark(),
            },
            ThemeEntry {
                id: STOCK_LIGHT,
                name: "Zeron Light",
                appearance: Appearance::Light,
                colors: ThemeColors::stock_light(),
            },
        ]
    })
}

/// Built-ins for one appearance, in display order.
pub fn built_ins_for(appearance: Appearance) -> Vec<&'static ThemeEntry> {
    built_ins()
        .iter()
        .filter(|entry| entry.appearance == appearance)
        .collect()
}

/// Look up a built-in by id.
pub fn entry_for(id: &str) -> Option<&'static ThemeEntry> {
    built_ins().iter().find(|entry| entry.id == id)
}

/// The id to use when nothing (or something unknown) is persisted for
/// `appearance` — always the stock palette.
pub fn default_id(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => STOCK_DARK,
        Appearance::Light => STOCK_LIGHT,
    }
}

/// Resolve a persisted id into a concrete entry. Unknown ids degrade to the
/// stock palette for the appearance rather than erroring — forward/backward
/// compatibility for settings files.
pub fn resolve(id: Option<&str>, appearance: Appearance) -> &'static ThemeEntry {
    let stock = default_id(appearance);
    id.filter(|id| !id.is_empty())
        .and_then(entry_for)
        .filter(|entry| entry.appearance == appearance)
        .unwrap_or_else(|| entry_for(stock).expect("stock ids are registered"))
}
