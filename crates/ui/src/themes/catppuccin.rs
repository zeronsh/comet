//! Catppuccin — Mocha (dark) and Latte (light).
//! https://github.com/catppuccin/catppuccin

use crate::theme::Appearance;

use super::{PaletteSpec, ThemeEntry, build, hex};

pub fn entries() -> Vec<ThemeEntry> {
    vec![
        ThemeEntry {
            id: "catppuccin-mocha",
            name: "Catppuccin Mocha",
            appearance: Appearance::Dark,
            colors: build(Appearance::Dark, mocha()),
        },
        ThemeEntry {
            id: "catppuccin-latte",
            name: "Catppuccin Latte",
            appearance: Appearance::Light,
            colors: build(Appearance::Light, latte()),
        },
    ]
}

fn mocha() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#1e1e2e"),              // base
        surface: hex("#181825"),         // mantle
        surface_card: hex("#313244"),    // surface0
        surface_dialog: hex("#45475a"),  // surface1
        surface_overlay: hex("#585b70"), // surface2
        text: hex("#cdd6f4"),
        text_muted: hex("#a6adc8"),     // subtext0
        text_faint: hex("#7f849c"),     // overlay1
        accent: hex("#b4befe"),         // lavender
        danger: hex("#f38ba8"),         // red
        warning: hex("#f9e2af"),        // yellow
        success: hex("#a6e3a1"),        // green
        busy: hex("#f5c2e7"),           // pink
        comment: hex("#6c7086"),        // overlay0
        code: hex("#cba6f7"),           // mauve
        syntax_keyword: hex("#cba6f7"), // mauve
        syntax_special: hex("#f5c2e7"), // pink
        syntax_string: hex("#a6e3a1"),  // green
        syntax_number: hex("#fab387"),  // peach
    }
}

fn latte() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#eff1f5"),           // base
        surface: hex("#e6e9ef"),      // mantle
        surface_card: hex("#ffffff"), // raised white
        surface_dialog: hex("#ffffff"),
        surface_overlay: hex("#ffffff"),
        text: hex("#4c4f69"),
        text_muted: hex("#6c6f85"),     // subtext0
        text_faint: hex("#8c8fa1"),     // overlay1
        accent: hex("#7287fd"),         // lavender
        danger: hex("#d20f39"),         // red
        warning: hex("#df8e1d"),        // yellow
        success: hex("#40a02b"),        // green
        busy: hex("#ea76cb"),           // pink
        comment: hex("#9ca0b0"),        // overlay0
        code: hex("#8839ef"),           // mauve
        syntax_keyword: hex("#8839ef"), // mauve
        syntax_special: hex("#ea76cb"), // pink
        syntax_string: hex("#40a02b"),  // green
        syntax_number: hex("#fe640b"),  // peach
    }
}
