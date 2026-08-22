//! Rosé Pine — Main (dark) and Dawn (light).
//! https://github.com/rose-pine/rose-pine

use crate::theme::Appearance;

use super::{PaletteSpec, ThemeEntry, build, hex};

pub fn entries() -> Vec<ThemeEntry> {
    vec![
        ThemeEntry {
            id: "rose-pine",
            name: "Rosé Pine",
            appearance: Appearance::Dark,
            colors: build(Appearance::Dark, main()),
        },
        ThemeEntry {
            id: "rose-pine-dawn",
            name: "Rosé Pine Dawn",
            appearance: Appearance::Light,
            colors: build(Appearance::Light, dawn()),
        },
    ]
}

fn main() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#191724"),              // base
        surface: hex("#1f1d2e"),         // surface
        surface_card: hex("#26233a"),    // overlay
        surface_dialog: hex("#403d52"),  // highlightMed
        surface_overlay: hex("#524f67"), // highlightHigh
        text: hex("#e0def4"),
        text_muted: hex("#908caa"),     // subtle
        text_faint: hex("#6e6a86"),     // muted
        accent: hex("#c4a7e7"),         // iris
        danger: hex("#eb6f92"),         // love
        warning: hex("#f6c177"),        // gold
        success: hex("#9ccfd8"),        // foam
        busy: hex("#ebbcba"),           // rose
        comment: hex("#6e6a86"),        // muted
        code: hex("#c4a7e7"),           // iris
        syntax_keyword: hex("#c4a7e7"), // iris
        syntax_special: hex("#eb6f92"), // love
        syntax_string: hex("#9ccfd8"),  // foam
        syntax_number: hex("#f6c177"),  // gold
    }
}

fn dawn() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#faf4ed"),      // base
        surface: hex("#f2e9e1"), // overlay (warm grey chrome)
        surface_card: hex("#ffffff"),
        surface_dialog: hex("#ffffff"),
        surface_overlay: hex("#ffffff"),
        text: hex("#575279"),
        text_muted: hex("#797593"),     // subtle
        text_faint: hex("#9893a5"),     // muted
        accent: hex("#907aa9"),         // iris
        danger: hex("#b4637a"),         // love
        warning: hex("#ea9d34"),        // gold
        success: hex("#56949f"),        // foam
        busy: hex("#d7827e"),           // rose
        comment: hex("#9893a5"),        // muted
        code: hex("#907aa9"),           // iris
        syntax_keyword: hex("#907aa9"), // iris
        syntax_special: hex("#b4637a"), // love
        syntax_string: hex("#56949f"),  // foam
        syntax_number: hex("#ea9d34"),  // gold
    }
}
