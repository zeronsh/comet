//! Theme registry: built-in theme families as [`ThemeColors`] data.
//!
//! A family contributes one [`ThemeEntry`] per appearance. The stock pair
//! (Zeron Dark / Light) keeps its hand-tuned palettes; every other family is
//! authored as a small [`PaletteSpec`] of role colors and expanded through
//! [`build`], which fills the elevation ladder, structural washes, and the
//! syntax palette from the family's accents.
//!
//! Selection model mirrors Zed's: the user picks an [`AppearanceMode`]
//! (system/light/dark) *and* which theme to use for each appearance. Unknown or
//! missing ids fall back to the stock palette so a settings file written by a
//! newer build never breaks boot.

use gpui::{hsla, Hsla};

use crate::theme::{mix, Appearance, SyntaxPalette, ThemeColors};

mod catppuccin;
mod rose_pine;
mod tokyo_night;

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

/// The hand-authored inputs a theme family provides. Everything else in
/// [`ThemeColors`] is derived by [`build`] so a family is a palette sketch, not
/// a 65-token table.
#[derive(Debug, Clone, Copy)]
pub struct PaletteSpec {
    /// Main content panel.
    pub bg: Hsla,
    /// Shell / sidebar chrome.
    pub surface: Hsla,
    /// Raised card resting on the panel (empty states, message bubbles).
    pub surface_card: Hsla,
    /// Modal dialog plane.
    pub surface_dialog: Hsla,
    /// Popover / menu / command-palette plane.
    pub surface_overlay: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    /// Primary accent (buttons, links, selection).
    pub accent: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    /// Streaming / working indicator.
    pub busy: Hsla,
    /// Inline-code and comment tone.
    pub comment: Hsla,
    /// Inline-code text color (a family's "purple"/"mauve" role).
    pub code: Hsla,
    // Syntax roles — fed straight into [`SyntaxPalette::from_roles`].
    pub syntax_keyword: Hsla,
    pub syntax_special: Hsla,
    pub syntax_string: Hsla,
    pub syntax_number: Hsla,
}

/// Expand a [`PaletteSpec`] into a full palette. Structural tokens (hairlines,
/// hover washes, selection alpha, caret, cursor, band, hunk wash) follow the
/// appearance, not the family — the stock rules for how dark vs. light UI
/// separates planes still apply. Accents, surfaces, and text are the family's.
pub fn build(appearance: Appearance, spec: PaletteSpec) -> ThemeColors {
    let dark = appearance.is_dark();

    let (element_hover, element_active, border, border_strong) = if dark {
        (
            hsla(0.0, 0.0, 0.92, 0.11),
            hsla(0.0, 0.0, 0.92, 0.16),
            hsla(0.0, 0.0, 1.0, 0.08),
            hsla(0.0, 0.0, 1.0, 0.14),
        )
    } else {
        (
            hsla(0.0, 0.0, 0.10, 0.06),
            hsla(0.0, 0.0, 0.10, 0.10),
            hsla(0.0, 0.0, 0.0, 0.10),
            hsla(0.0, 0.0, 0.0, 0.17),
        )
    };

    // Muted accent roles lean toward the family's text tone; that reads as
    // "softer same-hue" in dark (toward light text) and darkens in light.
    let toward_text = |color: Hsla| mix(color, spec.text, 0.35);

    ThemeColors {
        bg: spec.bg,
        surface: spec.surface,
        // Opaque pills/chips reuse the card plane; hover nudges its lightness
        // the same way the stock opaque-plate rule does (brighten on dark,
        // darken on light).
        surface_raised: spec.surface_card,
        surface_card: spec.surface_card,
        surface_dialog: spec.surface_dialog,
        surface_overlay: spec.surface_overlay,
        element_hover,
        element_active,
        border,
        border_strong,
        text: spec.text,
        text_muted: spec.text_muted,
        text_faint: spec.text_faint,
        text_dim: mix(spec.text_muted, spec.text_faint, 0.5),
        solid: spec.text,
        on_solid: spec.bg,
        accent: spec.accent,
        accent_strong: spec.accent,
        on_accent: spec.bg,
        danger: spec.danger,
        danger_muted: toward_text(spec.danger),
        warning: spec.warning,
        warning_muted: toward_text(spec.warning),
        success: spec.success,
        busy: spec.busy,
        success_muted: toward_text(spec.success),
        surface_raised_hover: shift_lightness(spec.surface_card, if dark { 0.05 } else { -0.05 }),
        band: if dark {
            hsla(0.0, 0.0, 0.0, 0.16)
        } else {
            hsla(0.0, 0.0, 0.0, 0.045)
        },
        input_bg: if dark {
            hsla(0.0, 0.0, 1.0, 0.03)
        } else {
            spec.bg
        },
        selection: spec.accent.opacity(if dark { 0.35 } else { 0.28 }),
        cursor: if dark {
            hsla(0.0, 0.0, 1.0, 0.35)
        } else {
            hsla(0.0, 0.0, 0.0, 0.55)
        },
        caret: spec.accent,
        danger_strong: spec.danger,
        code_text: spec.code,
        code_wash: spec.code.opacity(if dark { 0.12 } else { 0.10 }),
        syntax: SyntaxPalette::from_roles(
            spec.text,
            spec.comment,
            spec.danger,
            spec.syntax_keyword,
            spec.syntax_special,
            spec.syntax_string,
            spec.syntax_number,
        ),
        diff_add: spec.success,
        diff_del: spec.danger,
        diff_hunk_bg: if dark {
            hsla(0.6, 0.35, 0.6, 0.05)
        } else {
            hsla(0.6, 0.35, 0.35, 0.07)
        },
        glass_tint: None,
        glass_overlay_tint: None,
    }
}

/// Nudge a color's lightness by `delta` (clamped to the valid range).
fn shift_lightness(mut color: Hsla, delta: f32) -> Hsla {
    color.l = (color.l + delta).clamp(0.0, 1.0);
    color
}

/// Parse a `#RRGGBB` (or `#RRGGBBAA`) hex string. Panics on a bad literal —
/// palettes are authored at compile time, so a bad value is a code bug.
pub fn hex(s: &str) -> Hsla {
    Hsla::from(gpui::Rgba::try_from(s).expect("valid hex color literal"))
}

/// All built-in themes, one entry per (family, appearance), in display order.
pub fn built_ins() -> &'static [ThemeEntry] {
    static BUILT_INS: std::sync::OnceLock<Vec<ThemeEntry>> = std::sync::OnceLock::new();
    BUILT_INS.get_or_init(|| {
        let mut entries = vec![
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
        ];
        entries.extend(catppuccin::entries());
        entries.extend(tokyo_night::entries());
        entries.extend(rose_pine::entries());
        entries
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_has_both_appearances() {
        let entries = built_ins();
        for appearance in [Appearance::Dark, Appearance::Light] {
            let list = built_ins_for(appearance);
            assert!(
                list.len() >= 4,
                "expected stock + 3 families, got {}",
                list.len()
            );
            for entry in &list {
                assert_eq!(entry.appearance, appearance, "{} is misplaced", entry.id);
            }
        }
        // Ids are unique.
        let mut ids: Vec<_> = entries.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate theme ids");
    }

    #[test]
    fn resolve_prefers_exact_matches_and_degrades_to_stock() {
        assert_eq!(
            resolve(Some("catppuccin-mocha"), Appearance::Dark).id,
            "catppuccin-mocha"
        );
        assert_eq!(resolve(Some("nope"), Appearance::Dark).id, STOCK_DARK);
        assert_eq!(
            resolve(Some("catppuccin-mocha"), Appearance::Light).id,
            STOCK_LIGHT
        );
        assert_eq!(resolve(None, Appearance::Light).id, STOCK_LIGHT);
    }

    #[test]
    fn family_palettes_serialize_round_trip() {
        for entry in built_ins()
            .iter()
            .filter(|e| e.id != STOCK_DARK && e.id != STOCK_LIGHT)
        {
            let json = serde_json::to_string(&entry.colors).unwrap();
            let parsed: ThemeColors = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.glass_tint, None, "{} should derive glass", entry.id);
            assert_eq!(parsed.glass_overlay_tint, None);
        }
    }
}
