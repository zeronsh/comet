//! Tokyo Night — Night (dark) and Day (light).
//! https://github.com/folke/tokyonight.nvim

use crate::theme::Appearance;

use super::{build, hex, PaletteSpec, ThemeEntry};

pub fn entries() -> Vec<ThemeEntry> {
    vec![
        ThemeEntry {
            id: "tokyonight-night",
            name: "Tokyo Night",
            appearance: Appearance::Dark,
            colors: build(Appearance::Dark, night()),
        },
        ThemeEntry {
            id: "tokyonight-day",
            name: "Tokyo Day",
            appearance: Appearance::Light,
            colors: build(Appearance::Light, day()),
        },
    ]
}

fn night() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#1a1b26"),              // bg
        surface: hex("#16161e"),         // bg_dark (sidebar)
        surface_card: hex("#24283b"),    // between bg and highlight
        surface_dialog: hex("#292e42"),  // bg_highlight
        surface_overlay: hex("#3b4261"), // fg_gutter
        text: hex("#c0caf5"),            // fg
        text_muted: hex("#a9b1d6"),      // fg_dark
        text_faint: hex("#737aa2"),      // dark5
        accent: hex("#7aa2f7"),          // blue
        danger: hex("#f7768e"),          // red
        warning: hex("#e0af68"),         // yellow
        success: hex("#9ece6a"),         // green
        busy: hex("#bb9af7"),            // magenta
        comment: hex("#565f89"),         // comment
        code: hex("#9d7cd8"),            // purple
        syntax_keyword: hex("#7aa2f7"),  // blue
        syntax_special: hex("#bb9af7"),  // magenta
        syntax_string: hex("#9ece6a"),   // green
        syntax_number: hex("#ff9e64"),   // orange
    }
}

fn day() -> PaletteSpec {
    PaletteSpec {
        bg: hex("#e1e2e7"),      // bg
        surface: hex("#d0d5e3"), // bg_dark (sidebar)
        surface_card: hex("#ffffff"),
        surface_dialog: hex("#ffffff"),
        surface_overlay: hex("#ffffff"),
        text: hex("#3760bf"),           // fg
        text_muted: hex("#6172b0"),     // fg_dark
        text_faint: hex("#8990b3"),     // git.ignore
        accent: hex("#2e7de9"),         // blue
        danger: hex("#f52a65"),         // red
        warning: hex("#8c6c3e"),        // yellow
        success: hex("#587539"),        // green
        busy: hex("#9854f1"),           // magenta
        comment: hex("#848cb5"),        // comment
        code: hex("#7847bd"),           // purple
        syntax_keyword: hex("#2e7de9"), // blue
        syntax_special: hex("#9854f1"), // magenta
        syntax_string: hex("#587539"),  // green
        syntax_number: hex("#b15c00"),  // orange
    }
}
