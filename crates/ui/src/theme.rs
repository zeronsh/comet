//! The app theme — two concrete appearances, one token set.
//!
//! Colors are precomputed from an oklch-derived neutral scale (perceptually even
//! lightness steps; the same scale zeron's Tailwind theme used) into gpui [`Hsla`].
//! **Numbers drive layout, colors are paint**: layout constants live here as plain
//! numbers and never depend on which color is painted.
//!
//! # Light is designed, not inverted
//!
//! Mirroring lightness produces the classic "washed-out inverted" look, for three
//! reasons this module handles explicitly:
//!
//! 1. **Surface order flips meaning.** In dark, the main content panel is the
//!    *darkest* plane and raised surfaces get *lighter*. In light, the content
//!    panel is *white* and the shell/sidebar goes *grey* — chrome recedes by
//!    getting darker, not lighter. Popovers stay white and earn separation from a
//!    border and shadow rather than from lightness.
//! 2. **Elevation reverses.** On dark, a faint *white* wash means "raised". Its
//!    literal translation — a faint *black* wash on white — means "recessed", so
//!    the composer read as a dent instead of a plate. Light lifts with white plus
//!    a border and shadow ([`Theme::input_bg`], the elevation ladder). Fill
//!    *alphas* carry over unchanged ([`INK_FILL_SCALE`]); only hairlines scale, so
//!    a 1px edge survives a bright surround ([`INK_HAIRLINE_SCALE`]).
//! 3. **Accents must move down the scale.** The dark palette's 400-level accents
//!    (indigo/red/amber) are chosen for contrast against near-black; on white they
//!    fall to 2–4:1 and fail WCAG AA. Light mode uses the 600-level siblings at the
//!    same hue, which restores the *contrast ratio* the dark token had.
//!
//! Text tones are chosen so each light token lands within ~0.5 of its dark
//! counterpart's contrast ratio against its own background — the pairing is
//! verified in [`tests::text_contrast_is_paired_across_appearances`], not eyeballed.
//!
//! Installed as a gpui [`Global`] at boot; read with [`Theme::of`].

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use gpui::{hsla, App, Global, Hsla, SharedString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeron_syntax::HighlightKind;

/// Which appearance the app is painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub fn is_light(self) -> bool {
        matches!(self, Self::Light)
    }

    /// Map a gpui window appearance onto ours (both vibrant variants are just
    /// the blurred flavour of the same tone).
    pub fn from_window(appearance: gpui::WindowAppearance) -> Self {
        use gpui::WindowAppearance::*;
        match appearance {
            Light | VibrantLight => Self::Light,
            Dark | VibrantDark => Self::Dark,
        }
    }
}

/// Process-wide mirror of the installed theme's appearance.
///
/// The paint helpers ([`ink`], [`hairline`], [`wash`], …) are free functions
/// called from deep inside element builders that have no `cx` in scope, so they
/// read the appearance from here instead of the gpui global. Appearance is
/// genuinely process-wide — one setting for every window — so a single mirror is
/// sound; [`Theme::install`] is the only writer outside tests.
static CURRENT_APPEARANCE: AtomicU8 = AtomicU8::new(0);

/// Bumped every time the appearance actually changes.
///
/// Anything that caches *resolved colors* — most importantly the markdown
/// renderer's cross-frame `TextRun` cache, which bakes an `Hsla` into every run —
/// is only valid for the palette that produced it. Those caches were written when
/// the theme was a compile-time constant, so their validity keys cover content
/// only. Rather than thread the palette through every key, they compare this
/// counter and drop everything when it moves.
static THEME_GENERATION: AtomicU32 = AtomicU32::new(0);

/// The appearance the context-free paint helpers are painting for.
pub fn current_appearance() -> Appearance {
    match CURRENT_APPEARANCE.load(Ordering::Relaxed) {
        1 => Appearance::Light,
        _ => Appearance::Dark,
    }
}

/// Monotonic id of the current palette — see [`THEME_GENERATION`].
pub fn theme_generation() -> u32 {
    THEME_GENERATION.load(Ordering::Relaxed)
}

/// [`CURRENT_APPEARANCE`] is process-wide, so under the parallel test runner
/// any test that flips it — or asserts on the output of a helper that reads it
/// ([`ink`], [`hairline`], [`wash`], …) — must hold this lock. Crate-visible
/// because such tests exist outside this module too (see `motion::tests`).
/// Tests that flip the appearance restore Dark before releasing the guard.
#[cfg(test)]
pub(crate) fn lock_appearance() -> std::sync::MutexGuard<'static, ()> {
    static APPEARANCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    APPEARANCE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Point the context-free paint helpers at an appearance. Called by
/// [`Theme::install`]; exposed for tests that build a theme without an `App`.
pub fn set_current_appearance(appearance: Appearance) {
    let encoded = match appearance {
        Appearance::Dark => 0,
        Appearance::Light => 1,
    };
    if CURRENT_APPEARANCE.swap(encoded, Ordering::Relaxed) != encoded {
        THEME_GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Light-mode alpha multiplier for **fills** (hover/active washes, chip and pill
/// backgrounds).
///
/// This was 0.5 on the theory that dark ink on a bright field reads heavier and
/// should be scaled back. That theory is right for a *large* wash and badly wrong
/// for everything else: this palette leans on very low alphas for its subtle
/// fills — the composer plate is `ink(0.03)`, key caps are `ink(0.05)` — and
/// halving those produced 1.5% black on white, which is nothing. The composer
/// lost its background entirely and selected tabs stopped reading as selected.
///
/// The established light-UI scales (Primer, Radix) land subtle ≈ 3–4%, hover ≈ 8%,
/// selected ≈ 14% black — which is where the dark palette's white alphas already
/// sit. So the honest multiplier is 1: the same number in both appearances, with
/// only the *tone* flipping. Any per-state correction belongs in that state's
/// token, not in a blanket multiplier.
pub const INK_FILL_SCALE: f32 = 1.0;

/// Light-mode alpha multiplier for **hairlines** (borders, dividers, rings).
/// Opposite of fills: a 1px edge has to hold its own against a bright surround,
/// and the dark palette's white hairlines are deliberately faint. Scaling up
/// keeps separators legible instead of dissolving into the panel.
pub const INK_HAIRLINE_SCALE: f32 = 1.35;

/// Paint-only syntax colors. The hues follow the Git history graph's lane
/// palette (indigo, pink, emerald, amber, red, neutral), while light-mode
/// variants are darkened enough to remain readable as text on white.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxPalette {
    #[serde(with = "hex_color")]
    pub comment: Hsla,
    #[serde(with = "hex_color")]
    pub keyword: Hsla,
    #[serde(with = "hex_color")]
    pub string: Hsla,
    #[serde(with = "hex_color")]
    pub string_special: Hsla,
    #[serde(with = "hex_color")]
    pub escape: Hsla,
    #[serde(with = "hex_color")]
    pub number: Hsla,
    #[serde(with = "hex_color")]
    pub boolean: Hsla,
    #[serde(with = "hex_color")]
    pub type_name: Hsla,
    #[serde(with = "hex_color")]
    pub type_builtin: Hsla,
    #[serde(with = "hex_color")]
    pub constructor: Hsla,
    #[serde(with = "hex_color")]
    pub function: Hsla,
    #[serde(with = "hex_color")]
    pub function_builtin: Hsla,
    #[serde(with = "hex_color")]
    pub macro_name: Hsla,
    #[serde(with = "hex_color")]
    pub property: Hsla,
    #[serde(with = "hex_color")]
    pub constant: Hsla,
    #[serde(with = "hex_color")]
    pub variable: Hsla,
    #[serde(with = "hex_color")]
    pub variable_special: Hsla,
    #[serde(with = "hex_color")]
    pub parameter: Hsla,
    #[serde(with = "hex_color")]
    pub operator: Hsla,
    #[serde(with = "hex_color")]
    pub punctuation: Hsla,
    #[serde(with = "hex_color")]
    pub tag: Hsla,
    #[serde(with = "hex_color")]
    pub attribute: Hsla,
    #[serde(with = "hex_color")]
    pub label: Hsla,
    #[serde(with = "hex_color")]
    pub invalid: Hsla,
}

impl SyntaxPalette {
    pub fn color(&self, kind: HighlightKind) -> Hsla {
        match kind {
            HighlightKind::Comment => self.comment,
            HighlightKind::Keyword => self.keyword,
            HighlightKind::String => self.string,
            HighlightKind::StringSpecial => self.string_special,
            HighlightKind::Escape => self.escape,
            HighlightKind::Number => self.number,
            HighlightKind::Boolean => self.boolean,
            HighlightKind::Type => self.type_name,
            HighlightKind::TypeBuiltin => self.type_builtin,
            HighlightKind::Constructor => self.constructor,
            HighlightKind::Function => self.function,
            HighlightKind::FunctionBuiltin => self.function_builtin,
            HighlightKind::Macro => self.macro_name,
            HighlightKind::Property => self.property,
            HighlightKind::Constant => self.constant,
            HighlightKind::Variable => self.variable,
            HighlightKind::VariableSpecial => self.variable_special,
            HighlightKind::Parameter => self.parameter,
            HighlightKind::Operator => self.operator,
            HighlightKind::Punctuation | HighlightKind::Embedded => self.punctuation,
            HighlightKind::Tag => self.tag,
            HighlightKind::Attribute => self.attribute,
            HighlightKind::Label => self.label,
            HighlightKind::Invalid => self.invalid,
        }
    }

    fn dark(text: Hsla, comment: Hsla, danger: Hsla) -> Self {
        Self::from_roles(
            text,
            comment,
            danger,
            // Same sources as history::graph_color.
            oklch(0.673, 0.182, 276.935), // indigo — keywords, functions
            oklch(0.718, 0.202, 349.761), // pink — specials, tags
            oklch(0.765, 0.177, 163.223), // emerald — strings, constants
            oklch(0.828, 0.189, 84.429),  // amber — numbers, types
        )
    }

    fn light(text: Hsla, comment: Hsla, danger: Hsla) -> Self {
        Self::from_roles(
            text,
            comment,
            danger,
            oklch(0.47, 0.20, 276.966), // indigo
            oklch(0.47, 0.17, 0.584),   // pink
            oklch(0.46, 0.11, 163.225), // emerald
            oklch(0.47, 0.12, 48.998),  // amber
        )
    }

    /// Role-color constructor for theme families: pass the palette's own
    /// accents (keyword/special/string/number hues) and every input gets the
    /// [`git_graph_tone`] saturation treatment so code stays quieter than
    /// content.
    pub fn from_roles(
        text: Hsla,
        comment: Hsla,
        danger: Hsla,
        indigo: Hsla,
        pink: Hsla,
        emerald: Hsla,
        amber: Hsla,
    ) -> Self {
        let (indigo, pink) = (git_graph_tone(indigo), git_graph_tone(pink));
        let (emerald, amber) = (git_graph_tone(emerald), git_graph_tone(amber));
        let red = git_graph_tone(danger);
        Self {
            comment,
            keyword: indigo,
            string: emerald,
            string_special: pink,
            escape: pink,
            number: amber,
            boolean: amber,
            type_name: amber,
            type_builtin: emerald,
            constructor: amber,
            function: indigo,
            function_builtin: pink,
            macro_name: pink,
            property: amber,
            constant: emerald,
            variable: text,
            variable_special: pink,
            parameter: text,
            operator: text,
            punctuation: text,
            tag: pink,
            attribute: amber,
            label: amber,
            invalid: red,
        }
    }
}

/// Git history intentionally softens lane saturation so the graph remains
/// colorful without competing with content. Syntax uses the same treatment.
fn git_graph_tone(mut color: Hsla) -> Hsla {
    color.s *= 0.72;
    color
}

/// Serde support for [`Hsla`] as a hex string: `#RRGGBB` when opaque,
/// `#RRGGBBAA` when translucent. Round-trips through gpui's [`gpui::Rgba`]
/// `FromStr`, so any format it accepts parses.
mod hex_color {
    use gpui::{Hsla, Rgba};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(color: &Hsla, serializer: S) -> Result<S::Ok, S::Error> {
        let rgba = Rgba::from(*color);
        let [r, g, b, a] = [rgba.r, rgba.g, rgba.b, rgba.a].map(|v| (v * 255.0).round() as u8);
        let hex = if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        };
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Hsla, D::Error> {
        let text = String::deserialize(deserializer)?;
        let rgba = Rgba::try_from(text.as_str()).map_err(serde::de::Error::custom)?;
        Ok(Hsla::from(rgba))
    }

    pub fn serialize_option<S: Serializer>(
        color: &Option<Hsla>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match color {
            Some(color) => serialize(color, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize_option<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Hsla>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        match text {
            None => Ok(None),
            Some(text) => {
                let rgba = Rgba::try_from(text.as_str()).map_err(serde::de::Error::custom)?;
                Ok(Some(Hsla::from(rgba)))
            }
        }
    }
}

/// One theme's palette as plain data — every paint token of [`Theme`], plus the
/// optional glass overrides, serializable to JSON with hex colors.
///
/// This is the unit the registry and (later) on-disk custom themes traffic in;
/// [`Theme`] itself stays the runtime view (adds fonts + appearance and the
/// derived-glass methods).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeColors {
    // ---- paint: neutral surfaces ----
    #[serde(with = "hex_color")]
    pub bg: Hsla,
    #[serde(with = "hex_color")]
    pub surface: Hsla,
    #[serde(with = "hex_color")]
    pub surface_raised: Hsla,
    // ---- paint: elevation ladder ----
    #[serde(with = "hex_color")]
    pub surface_card: Hsla,
    #[serde(with = "hex_color")]
    pub surface_dialog: Hsla,
    #[serde(with = "hex_color")]
    pub surface_overlay: Hsla,
    #[serde(with = "hex_color")]
    pub element_hover: Hsla,
    #[serde(with = "hex_color")]
    pub element_active: Hsla,
    #[serde(with = "hex_color")]
    pub border: Hsla,
    #[serde(with = "hex_color")]
    pub border_strong: Hsla,
    // ---- paint: text ----
    #[serde(with = "hex_color")]
    pub text: Hsla,
    #[serde(with = "hex_color")]
    pub text_muted: Hsla,
    #[serde(with = "hex_color")]
    pub text_faint: Hsla,
    #[serde(with = "hex_color")]
    pub text_dim: Hsla,
    // ---- paint: high-contrast solid ----
    #[serde(with = "hex_color")]
    pub solid: Hsla,
    #[serde(with = "hex_color")]
    pub on_solid: Hsla,
    // ---- paint: accents ----
    #[serde(with = "hex_color")]
    pub accent: Hsla,
    #[serde(with = "hex_color")]
    pub accent_strong: Hsla,
    #[serde(with = "hex_color")]
    pub on_accent: Hsla,
    #[serde(with = "hex_color")]
    pub danger: Hsla,
    #[serde(with = "hex_color")]
    pub danger_muted: Hsla,
    #[serde(with = "hex_color")]
    pub warning: Hsla,
    #[serde(with = "hex_color")]
    pub warning_muted: Hsla,
    #[serde(with = "hex_color")]
    pub success: Hsla,
    #[serde(with = "hex_color")]
    pub busy: Hsla,
    #[serde(with = "hex_color")]
    pub success_muted: Hsla,
    // ---- paint: components ----
    #[serde(with = "hex_color")]
    pub surface_raised_hover: Hsla,
    #[serde(with = "hex_color")]
    pub band: Hsla,
    #[serde(with = "hex_color")]
    pub input_bg: Hsla,
    #[serde(with = "hex_color")]
    pub selection: Hsla,
    #[serde(with = "hex_color")]
    pub cursor: Hsla,
    #[serde(with = "hex_color")]
    pub caret: Hsla,
    #[serde(with = "hex_color")]
    pub danger_strong: Hsla,
    // ---- paint: code & diff ----
    #[serde(with = "hex_color")]
    pub code_text: Hsla,
    #[serde(with = "hex_color")]
    pub code_wash: Hsla,
    #[serde(
        serialize_with = "syntax_serialize",
        deserialize_with = "syntax_deserialize"
    )]
    pub syntax: SyntaxPalette,
    #[serde(with = "hex_color")]
    pub diff_add: Hsla,
    #[serde(with = "hex_color")]
    pub diff_del: Hsla,
    #[serde(with = "hex_color")]
    pub diff_hunk_bg: Hsla,
    // ---- glass overrides ----
    /// Frost tint over the blurred desktop. `None` derives from [`Self::surface`]
    /// at the platform frost alpha; built-in palettes pin their sampled greys so
    /// stock appearances stay pixel-identical.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "hex_color::serialize_option",
        deserialize_with = "hex_color::deserialize_option"
    )]
    pub glass_tint: Option<Hsla>,
    /// Translucent tint floating cards paint over their backdrop blur. `None`
    /// keeps the per-appearance recipe in [`Theme::glass_overlay`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "hex_color::serialize_option",
        deserialize_with = "hex_color::deserialize_option"
    )]
    pub glass_overlay_tint: Option<Hsla>,
}

fn syntax_serialize<S: Serializer>(
    syntax: &SyntaxPalette,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    syntax.serialize(serializer)
}

fn syntax_deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SyntaxPalette, D::Error> {
    SyntaxPalette::deserialize(deserializer)
}

impl ThemeColors {
    /// Stock dark palette. Surface tones are sampled straight from the
    /// reference screenshots of the original app (docs/reference): main panel
    /// `#060606`, shell/sidebar `#0d0d0d`. `glass_tint` pins the sampled
    /// frost grey so stock dark stays pixel-identical to the pre-theme build.
    pub fn stock_dark() -> Self {
        Self {
            bg: grey(6),       // main panel — sampled #060606
            surface: grey(13), // shell / sidebar — sampled #0d0d0d
            surface_raised: neutral(0.235),
            surface_card: grey(0x0e),
            surface_dialog: grey(0x10),
            surface_overlay: grey(0x16),
            element_hover: hsla(0.0, 0.0, 0.92, 0.11),
            element_active: hsla(0.0, 0.0, 0.92, 0.16),
            border: hsla(0.0, 0.0, 1.0, 0.08),
            border_strong: hsla(0.0, 0.0, 1.0, 0.14),
            text: neutral(0.922),       // ~neutral-200
            text_muted: neutral(0.708), // ~neutral-400
            text_faint: neutral(0.556), // ~neutral-500
            text_dim: grey(0x98),
            solid: neutral(0.922),                       // near-white plate
            on_solid: grey(0x0e),                        // near-black label
            accent: oklch(0.673, 0.182, 276.935),        // indigo-400
            accent_strong: oklch(0.585, 0.233, 277.117), // indigo-500
            on_accent: neutral(0.985),
            danger: oklch(0.704, 0.191, 22.216),       // red-400
            danger_muted: oklch(0.808, 0.114, 19.571), // red-300
            warning: oklch(0.828, 0.189, 84.429),      // amber-400
            warning_muted: oklch(0.924, 0.12, 95.746), // amber-200
            success: oklch(0.765, 0.177, 163.223),     // emerald-400
            busy: oklch(0.718, 0.202, 349.761),        // pink-400
            success_muted: oklch(0.845, 0.143, 164.978), // emerald-300
            surface_raised_hover: neutral(0.29),
            band: band_for(Appearance::Dark),
            input_bg: hsla(0.0, 0.0, 1.0, 0.03),
            selection: hsla(0.66, 0.6, 0.55, 0.35),
            cursor: hsla(0.0, 0.0, 1.0, 0.35),
            caret: hsla(0.66, 0.7, 0.7, 1.0),
            danger_strong: oklch(0.58, 0.16, 25.0),
            code_text: oklch(0.811, 0.111, 293.571), // violet-300
            code_wash: oklch(0.702, 0.183, 293.541).opacity(0.12), // violet-400/12
            syntax: SyntaxPalette::dark(neutral(0.922), neutral(0.60), oklch(0.704, 0.191, 22.216)),
            diff_add: oklch(0.765, 0.177, 163.223), // emerald-400
            diff_del: oklch(0.704, 0.191, 22.216),  // red-400
            diff_hunk_bg: hsla(0.6, 0.35, 0.6, 0.05),
            glass_tint: Some(grey(8)),
            glass_overlay_tint: None,
        }
    }

    /// Stock light palette.
    pub fn stock_light() -> Self {
        Self {
            bg: grey(0xff), // main panel — clean white
            // Deeper than ~neutral-100 looks on paper: the content card is pure
            // white and sits *inside* this surface, so too small a step leaves
            // the whole window one flat sheet with a hairline drawn on it.
            surface: neutral(0.968),
            // A real grey, NOT white. This is the opaque-plate tone — user
            // message bubbles, the jump-to-bottom pill — and those sit directly
            // on the white content plane with no border or shadow to save them.
            // White here made the user's own messages vanish into the page.
            surface_raised: neutral(0.940),
            surface_card: grey(0xff),
            surface_dialog: grey(0xff),
            surface_overlay: grey(0xff),
            element_hover: hsla(0.0, 0.0, 0.10, 0.06),
            element_active: hsla(0.0, 0.0, 0.10, 0.10),
            border: hsla(0.0, 0.0, 0.0, 0.10),
            border_strong: hsla(0.0, 0.0, 0.0, 0.17),
            // ~neutral-850. Backing off from pure neutral-900 keeps the same
            // perceived weight as the dark theme rather than maximum contrast.
            text: neutral(0.25),
            text_muted: neutral(0.439), // ~neutral-600 → ~7.7:1
            text_faint: neutral(0.535),
            text_dim: neutral(0.50),
            solid: neutral(0.205),    // near-black plate, deeper than body text
            on_solid: neutral(0.985), // near-white label
            accent: oklch(0.511, 0.262, 276.966), // indigo-600
            accent_strong: oklch(0.511, 0.262, 276.966), // indigo-600 fill
            on_accent: neutral(0.985),
            danger: oklch(0.577, 0.245, 27.325),        // red-600
            danger_muted: oklch(0.505, 0.213, 27.518),  // red-700
            warning: oklch(0.555, 0.163, 48.998),       // amber-700 — carries 12px text
            warning_muted: oklch(0.473, 0.137, 46.201), // amber-800
            success: oklch(0.596, 0.145, 163.225),      // emerald-600
            busy: oklch(0.592, 0.249, 0.584),           // pink-600
            success_muted: oklch(0.508, 0.118, 165.612), // emerald-700
            // Opaque pills darken on hover here rather than brighten — same
            // "brighten the plate, don't wash it out" rule, read the other way.
            surface_raised_hover: neutral(0.900),
            band: band_for(Appearance::Light),
            input_bg: grey(0xff),
            selection: hsla(0.66, 0.75, 0.62, 0.28),
            cursor: hsla(0.0, 0.0, 0.0, 0.55),
            caret: hsla(0.66, 0.78, 0.42, 1.0),
            danger_strong: oklch(0.51, 0.20, 25.0),
            code_text: oklch(0.491, 0.27, 292.581), // violet-700
            code_wash: oklch(0.541, 0.281, 293.009).opacity(0.10), // violet-600/10
            syntax: SyntaxPalette::light(neutral(0.25), neutral(0.48), oklch(0.505, 0.213, 27.518)),
            diff_add: oklch(0.596, 0.145, 163.225), // emerald-600
            diff_del: oklch(0.577, 0.245, 27.325),  // red-600
            diff_hunk_bg: hsla(0.6, 0.35, 0.35, 0.07),
            glass_tint: Some(grey(0xfa)),
            glass_overlay_tint: None,
        }
    }
}

/// The app theme. Built from a [`ThemeColors`] palette plus fonts; the stock
/// instances are [`Theme::dark`] and [`Theme::light`].
#[derive(Debug, Clone)]
pub struct Theme {
    /// Registry id of the palette this theme was built from (e.g.
    /// `"zeron-dark"`). Used to detect actual theme changes in
    /// [`crate::appearance::apply`] and to persist the selection.
    pub id: SharedString,
    /// Human-readable name (Settings picker label).
    pub name: SharedString,
    /// Which appearance these tokens were built for.
    pub appearance: Appearance,

    // ---- paint: neutral surfaces ----
    /// Main content panel. Dark: the deepest plane (#060606). Light: pure white —
    /// long-form content reads best on an unbroken white field.
    pub bg: Hsla,
    /// Shell / sidebar surface. Dark: one step *up* from `bg`. Light: one step
    /// *down* (grey) — chrome recedes from the content plane in both, which is
    /// the direction a naive invert gets backwards.
    pub surface: Hsla,
    /// Raised surface: opaque pills and chips that sit proud of the panel.
    /// Dark: lighter than `surface`. Light: white, separated by `border` +
    /// shadow rather than by lightness.
    pub surface_raised: Hsla,

    // ---- paint: elevation ladder ----
    //
    // Dark mode distinguishes floating planes by lightness, and the steps are
    // *small* (#0e → #10 → #16 → #1e). They are not interchangeable: collapsing
    // them onto one token visibly lifts popovers off their intended plane.
    //
    // Light mode cannot use the same trick, because the content plane is already
    // white and there is nothing lighter to climb to. All three land on white and
    // let `border` + shadow carry the separation instead — the standard light-UI
    // answer, and the reason this is a ladder of tokens rather than an arithmetic
    // offset applied to one.
    /// Inline card resting on the main panel (auth gate, empty-state cards).
    pub surface_card: Hsla,
    /// Modal dialog, floating over a [`Theme::scrim`].
    pub surface_dialog: Hsla,
    /// Popover, menu and command-palette surface — the highest plane.
    pub surface_overlay: Hsla,
    /// Hover wash for interactive rows/buttons.
    pub element_hover: Hsla,
    /// Active/selected wash.
    pub element_active: Hsla,
    /// Hairline border.
    pub border: Hsla,
    /// Stronger border for focused/raised edges.
    pub border_strong: Hsla,

    // ---- paint: text ----
    /// Primary text. ~17.5:1 on its own background in both appearances.
    pub text: Hsla,
    /// Muted text: timestamps, secondary labels. ~7.5–8:1.
    pub text_muted: Hsla,
    /// Faint text: placeholders, disabled. ~4.5:1 — AA for body copy.
    pub text_faint: Hsla,
    /// One notch below `text_muted` — the diff file-path tone. It exists as its
    /// own token rather than being folded into `text_muted` because the dark
    /// value was sampled (#989898) and folding it would shift that label, which
    /// is a palette change dressed up as a refactor.
    pub text_dim: Hsla,

    // ---- paint: high-contrast solid (primary buttons) ----
    /// The maximum-contrast solid fill: near-white on dark, near-black on light.
    /// This is the primary button plate.
    pub solid: Hsla,
    /// Label/icon color on top of [`Self::solid`] — its inverse.
    pub on_solid: Hsla,

    // ---- paint: accents ----
    /// Accent — indigo. Text/icon weight: indigo-400 on dark, indigo-600 on light
    /// (the 400 fails AA on white).
    pub accent: Hsla,
    /// Stronger accent for fills that carry [`Self::on_accent`] text.
    pub accent_strong: Hsla,
    /// Label color on top of [`Self::accent_strong`].
    pub on_accent: Hsla,
    /// Danger — red (errors, stop button).
    pub danger: Hsla,
    /// Softer danger for secondary/inline error copy.
    pub danger_muted: Hsla,
    /// Warning — amber (offline notices, awaiting-input).
    pub warning: Hsla,
    /// Softer warning for secondary copy.
    pub warning_muted: Hsla,
    /// Success / online — emerald.
    pub success: Hsla,
    /// Working / streaming indicator — pink.
    pub busy: Hsla,
    /// Softer success for text on a success-tinted chip.
    pub success_muted: Hsla,

    // ---- paint: components ----
    /// Hover tone for an *opaque* raised pill. Hover must brighten the plate in
    /// dark mode, never swap it for a translucent wash (that made pills go
    /// see-through — user-reported); in light mode it darkens instead, same idea.
    pub surface_raised_hover: Hsla,
    /// Recessed band behind a palette/picker header or footer strip. Translucent
    /// so the glass still reads through.
    pub band: Hsla,
    /// The composer pill and other input plates.
    ///
    /// Its own token because "lifted" inverts between appearances. On dark, a
    /// faint *white* wash over near-black reads as raised. The literal light
    /// translation — a faint *black* wash on white — reads as **recessed**, a dent
    /// rather than a plate, which is why the prompt looked like bare text on a
    /// smudge. Light mode lifts the way light UIs actually do: pure white, with
    /// the border and shadow carrying the elevation.
    pub input_bg: Hsla,
    /// Text-selection highlight in the composer and inputs.
    pub selection: Hsla,
    /// Terminal block cursor.
    pub cursor: Hsla,
    /// Composer text caret. A blue distinct from `accent` — sampled from the
    /// original composer, not derived, so it keeps its own token.
    pub caret: Hsla,
    /// Destructive-action button fill (danger plate, carries [`Self::on_accent`]).
    pub danger_strong: Hsla,

    // ---- paint: code & diff ----
    /// Inline-code text — violet, per the user's "a nice purple" request.
    pub code_text: Hsla,
    /// Inline-code wash behind [`Self::code_text`].
    pub code_wash: Hsla,
    /// Shared paint-only syntax palette.
    pub syntax: SyntaxPalette,
    /// Diff: added lines.
    pub diff_add: Hsla,
    /// Diff: deleted lines.
    pub diff_del: Hsla,
    /// Diff: hunk-header wash (bluish grey).
    pub diff_hunk_bg: Hsla,

    // ---- fonts ----
    /// UI font family (bundling of Geist lands with asset work; until then the
    /// text system falls back to the system sans when the family is missing).
    pub font_sans: SharedString,
    /// Monospace family for code/terminal.
    pub font_mono: SharedString,
    /// Explicit system fallbacks, for callers that want to skip the lookup.
    pub font_sans_fallback: SharedString,
    pub font_mono_fallback: SharedString,

    // ---- glass overrides (from the palette; see [`ThemeColors::glass_tint`]) ----
    /// `None` = derived from [`Self::surface`] at the frost alpha.
    pub glass_tint: Option<Hsla>,
    /// `None` = the per-appearance recipe in [`Self::glass_overlay`].
    pub glass_overlay_tint: Option<Hsla>,
    /// Frost opacity (0.0..1.0). `1.0` disables the frosted glass — the chrome
    /// paints the opaque [`Self::surface`] instead. Defaults to the platform's
    /// [`Self::GLASS_ALPHA`]; a global "frost strength" setting overrides it at
    /// install time.
    pub glass_alpha: f32,
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    /// Opaque elsewhere: Linux/Windows get no compositor-blur guarantee, and a
    /// merely transparent window would show raw desktop through the sidebar.
    /// Darkness matched by eye to a reference Electron app's dark glass. That
    /// scrim is 0.76 over `hsl(0 0% 3%)`, but it sits on Electron's
    /// `under-window` vibrancy MATERIAL, which pre-darkens the blur; our bare
    /// backdrop blur has no material layer, so the scrim runs heavier to land
    /// on the same perceived tone (see [`Theme::glass`]).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Light-mode frost alpha — glass-forward, like dark mode.
    ///
    /// A light tint controls the blur less than a dark one: the desktop's
    /// colour bleeds through more readily, so light frost runs *heavier* than
    /// an equal-looking dark frost to keep the chrome on a known-enough
    /// background for its labels (macOS light sidebars do the same — their
    /// vibrancy material is mostly white). Floating cards compensate further:
    /// see [`Self::glass_overlay`], where light coverage steps up to keep menu
    /// text legible over an unknown backdrop.
    pub const GLASS_ALPHA_LIGHT: f32 = if cfg!(target_os = "macos") { 0.80 } else { 1.0 };
    /// Main-panel header height (zeron `h-11`) — in-card headers (changes pane).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs). Content
    /// rides [`Self::TITLEBAR_TOP_PAD`] lower than center so the air above
    /// matches the perceived gap to the inset card below (border + card body).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet (zeron `h-6`) — the
    /// WorkingIndicator row; reserving it keeps the composer from shifting.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;
    /// Height of the gradient that fades the transcript into the panel
    /// background at its bottom edge. The transcript's last row must pad
    /// itself past this band so settled content (message text, the
    /// hover-revealed timestamp) never sits inside the fade when scrolled
    /// to the bottom.
    pub const TRANSCRIPT_FADE_BAND: f32 = 24.0;
    /// Message bubble corner radius.
    pub const BUBBLE_RADIUS: f32 = 16.0;
    /// Panel / card corner radius.
    pub const PANEL_RADIUS: f32 = 10.0;
    /// Small control radius (buttons, chips).
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;
    /// Optical separation for a tightly coupled title/description stack.
    /// This is intentionally outside the base spacing ladder: it corrects
    /// line-box whitespace rather than separating layout regions.
    pub const TEXT_STACK_GAP: f32 = 1.0;

    /// The frost tint painted over the blurred window background (macOS glass).
    /// Built-in palettes pin their sampled greys via [`Self::glass_tint`]; an
    /// un-overridden theme derives its frost from its own [`Self::surface`].
    /// On opaque platforms — or when [`Self::glass_alpha`] reaches `1.0` — this
    /// IS the surface tone (no tint swap).
    pub fn glass(&self) -> Hsla {
        if self.glass_alpha >= 1.0 {
            self.surface
        } else {
            self.glass_tint
                .unwrap_or(self.surface)
                .opacity(self.glass_alpha)
        }
    }

    /// Whether this appearance paints translucent chrome over the blurred
    /// desktop. Glass-only recipes — backdrop blurs, translucent popover
    /// tints, per-glyph edge fades — must gate on this, not on
    /// [`Self::GLASS_ALPHA`]: that constant is platform-wide, while the frost
    /// alpha (and with it whether glass is on at all) is per-appearance.
    pub fn is_glass(&self) -> bool {
        self.glass().a < 1.0
    }

    /// Whether FLOATING surfaces (popovers, the composer pill) paint their
    /// backdrop blur and translucent tints. Unlike [`Self::is_glass`] this is
    /// scene-level: the blur runs on in-app content inside the window, not on
    /// the desktop behind it, so it needs no compositor vibrancy — macOS
    /// rasterizes it in Metal and Linux in the vendored wgpu renderer (other
    /// wgpu platforms keep opaque floats until tested). The window chrome
    /// itself stays opaque off macOS either way.
    pub fn is_frost(&self) -> bool {
        cfg!(any(target_os = "macos", target_os = "linux"))
    }

    /// Hover wash for chrome that sits ON GLASS (sidebar rows, tabs, titlebar
    /// buttons). One recipe, both appearances: the 11% [`wash`], tone-flipped
    /// by the palette convention (soft-white on dark, soft-black on light).
    ///
    /// Hover and selection share the SAME fill (selection adds only the ring).
    /// Light previously ran heavy white washes here (hover 0.55, selection
    /// 0.92) after a black-hover-next-to-white-selection mismatch report; now
    /// hover and selection are *both* the tone-flipped wash, so they lift the
    /// same way again. Light's alpha sits under dark's: dark's 11% at the
    /// light tone read too dark over the bright frost (user report).
    pub fn glass_hover(&self) -> Hsla {
        match self.appearance {
            Appearance::Dark => wash_for(Appearance::Dark, 0.11),
            Appearance::Light => wash_for(Appearance::Light, 0.06),
        }
    }

    /// The translucent tint floating cards paint over their backdrop blur
    /// (see [`crate::frost::frosted`]). Dark: the reference zeron
    /// `.glass-surface` menu tint verbatim — `oklch(0.33 0 0 / 34%)`. The
    /// previous `surface_overlay` at 65% was tuned back when the tint had to
    /// *approximate* the composited recipe without a real blur; kept over the
    /// blur it buried the backdrop's colour and menus read as flat grey slabs
    /// next to the hue-inheriting chrome (user report). At 34% the blurred
    /// backdrop carries the card and the mid-grey only lifts it off the
    /// plane. Light: heavier — a translucent white tint left menu text
    /// ghosting over whatever sat behind the popover, so light coverage
    /// steps up to keep rows on a known background.
    pub fn glass_overlay(&self) -> Hsla {
        if let Some(tint) = self.glass_overlay_tint {
            return tint;
        }
        match self.appearance {
            Appearance::Dark => oklch(0.33, 0.0, 0.0).opacity(0.34),
            Appearance::Light => self.surface_overlay.opacity(0.85),
        }
    }

    /// The composer pill / question panel fill. Light's `input_bg` is opaque
    /// white (the elevation ladder on an opaque page) — over glass it read as
    /// a solid slab in front of the frosted blur, so it thins to a
    /// translucent tint there (0.6 and then 0.45 both still read too bright
    /// over the 0.80 frost — lowered on user request). Dark's 3% white wash
    /// is already glass-native.
    pub fn input_glass_bg(&self) -> Hsla {
        if self.is_frost() && matches!(self.appearance, Appearance::Light) {
            self.input_bg.opacity(0.30)
        } else {
            self.input_bg
        }
    }

    /// Section-card fill (settings cards and similar in-panel cards). The
    /// opaque `surface` tone read as a harsh solid slab floating on the
    /// frosted blur (user report), so glass thins it to a translucent tint;
    /// opaque platforms keep the true card tone.
    pub fn card_glass_bg(&self) -> Hsla {
        if self.is_glass() {
            self.surface.opacity(0.40)
        } else {
            self.surface
        }
    }

    /// The standard modal backdrop — see [`scrim`].
    pub fn scrim(&self) -> Hsla {
        scrim_for(self.appearance, SCRIM_ALPHA_DARK)
    }

    /// How the platform should composite the window behind our paint.
    ///
    /// Only dark macOS wants the blurred desktop — light chrome is opaque by
    /// design ([`Self::GLASS_ALPHA_LIGHT`]), so it keeps opaque compositing
    /// (subpixel-friendly, no vibrancy cost for a blur nothing shows). This is
    /// a method rather than a constant because it has to be *re-applied* after
    /// every theme swap: gpui's macOS backend tears the `NSVisualEffectView`
    /// out of the hierarchy whenever the value is anything but `Blurred`, and
    /// the re-apply in `appearance::apply` is what restores vibrancy when the
    /// user switches back to dark. See zed's `crates/zed/src/main.rs`, which
    /// runs the same loop on every settings change.
    pub fn window_background_appearance(&self) -> gpui::WindowBackgroundAppearance {
        if self.is_glass() {
            gpui::WindowBackgroundAppearance::Blurred
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        }
    }

    /// Build the dark theme from the stock palette (see [`ThemeColors::stock_dark`]).
    pub fn dark() -> Self {
        Self::from_colors(
            ThemeColors::stock_dark(),
            crate::themes::STOCK_DARK,
            "Zeron Dark",
            Appearance::Dark,
        )
    }

    /// Build the light theme (see [`ThemeColors::stock_light`]).
    pub fn light() -> Self {
        Self::from_colors(
            ThemeColors::stock_light(),
            crate::themes::STOCK_LIGHT,
            "Zeron Light",
            Appearance::Light,
        )
    }

    /// Build a runtime theme from palette data. Fonts are app-level choices,
    /// not palette — every theme shares the same families.
    pub fn from_colors(colors: ThemeColors, id: &str, name: &str, appearance: Appearance) -> Self {
        let ThemeColors {
            bg,
            surface,
            surface_raised,
            surface_card,
            surface_dialog,
            surface_overlay,
            element_hover,
            element_active,
            border,
            border_strong,
            text,
            text_muted,
            text_faint,
            text_dim,
            solid,
            on_solid,
            accent,
            accent_strong,
            on_accent,
            danger,
            danger_muted,
            warning,
            warning_muted,
            success,
            busy,
            success_muted,
            surface_raised_hover,
            band,
            input_bg,
            selection,
            cursor,
            caret,
            danger_strong,
            code_text,
            code_wash,
            syntax,
            diff_add,
            diff_del,
            diff_hunk_bg,
            glass_tint,
            glass_overlay_tint,
        } = colors;
        Self {
            id: id.into(),
            name: name.into(),
            appearance,
            bg,
            surface,
            surface_raised,
            surface_card,
            surface_dialog,
            surface_overlay,
            element_hover,
            element_active,
            border,
            border_strong,
            text,
            text_muted,
            text_faint,
            text_dim,
            solid,
            on_solid,
            accent,
            accent_strong,
            on_accent,
            danger,
            danger_muted,
            warning,
            warning_muted,
            success,
            busy,
            success_muted,
            surface_raised_hover,
            band,
            input_bg,
            selection,
            cursor,
            caret,
            danger_strong,
            code_text,
            code_wash,
            syntax,
            diff_add,
            diff_del,
            diff_hunk_bg,
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
            glass_tint,
            glass_overlay_tint,
            glass_alpha: Self::GLASS_ALPHA,
        }
    }

    /// Build the theme for an appearance.
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    /// Install the stock theme for `appearance`. See [`Self::install_theme`].
    pub fn install(appearance: Appearance, cx: &mut App) {
        Self::install_theme(Self::for_appearance(appearance), cx);
    }

    /// Install a concrete theme as the gpui global and point the context-free
    /// paint helpers at its appearance. The **only** way the palette should
    /// change — setting the global directly leaves [`current_appearance`] stale.
    pub fn install_theme(theme: Self, cx: &mut App) {
        set_current_appearance(theme.appearance);
        // Bump the generation even when the appearance did not move: caches
        // keyed on content alone (the markdown renderer's cross-frame TextRun
        // cache bakes an Hsla into every run) must drop on ANY palette swap,
        // including dark-theme → different-dark-theme.
        THEME_GENERATION.fetch_add(1, Ordering::Relaxed);
        cx.set_global(theme);
    }

    /// Read the theme global.
    pub fn of(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    /// Overlay ink at `alpha` — see [`ink`].
    pub fn ink(&self, alpha: f32) -> Hsla {
        ink_for(self.appearance, alpha)
    }

    /// Hairline ink at `alpha` — see [`hairline`].
    pub fn hairline(&self, alpha: f32) -> Hsla {
        hairline_for(self.appearance, alpha)
    }

    /// State wash at `alpha` — see [`wash`].
    pub fn wash(&self, alpha: f32) -> Hsla {
        wash_for(self.appearance, alpha)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

fn system_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn system_mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A neutral (chroma 0) oklch tone as Hsla. Chroma 0 means r == g == b exactly,
/// so this goes straight to an achromatic Hsla (skipping the hue math avoids
/// float-noise saturation).
pub fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Translucent **fill** ink for interactive states and chip plates: soft-white on
/// dark, soft-black on light at [`INK_FILL_SCALE`] of the alpha.
///
/// Alphas are quoted in *dark-mode terms* at every call site — the dark theme is
/// the tuned one — and the light value is derived. Callers keep one number and
/// both appearances stay in the relationship the dark tuning established.
///
/// Fills must never rest on transparent BLACK in dark mode: fully opaque washes
/// killed the glass and flashed dark mid-fade (user reports), so hover fades rest
/// on `ink(0.0)`, which stays tonally correct at zero alpha.
pub fn ink(alpha: f32) -> Hsla {
    ink_for(current_appearance(), alpha)
}

fn ink_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        // Soft-white, not pure white: alphas are high enough to stay visible at
        // the brightest backdrop the 0.90 glass scrim can produce.
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, alpha * INK_FILL_SCALE),
    }
}

/// Translucent **hairline** ink for borders, dividers and rings: white on dark,
/// black on light at [`INK_HAIRLINE_SCALE`] of the alpha.
///
/// Separate from [`ink`] because edges and fills scale in opposite directions
/// when the field brightens — a 1px line needs *more* ink on white, a plate needs
/// less.
pub fn hairline(alpha: f32) -> Hsla {
    hairline_for(current_appearance(), alpha)
}

fn hairline_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 1.0, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.0, (alpha * INK_HAIRLINE_SCALE).min(0.5)),
    }
}

/// Interactive-state wash: a softened [`ink`] that stops short of pure black or
/// white so hover plates read as tinted glass rather than paint.
pub fn wash(alpha: f32) -> Hsla {
    wash_for(current_appearance(), alpha)
}

fn wash_for(appearance: Appearance, alpha: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.92, alpha),
        Appearance::Light => hsla(0.0, 0.0, 0.10, alpha * INK_FILL_SCALE),
    }
}

/// Alpha of the standard modal backdrop in dark mode. Call sites that need a
/// heavier or lighter scrim pass their own dark-mode alpha to [`scrim`].
pub const SCRIM_ALPHA_DARK: f32 = 0.60;

/// Modal backdrop at `alpha_dark` (quoted, as everywhere, in dark-mode terms).
///
/// Black in both appearances — a scrim's job is to darken what is behind it, and
/// a "light scrim" of white would wash the modal out rather than seat it. What
/// changes is strength: on a bright field a dark-mode-weight scrim reads as a
/// blackout, so light mode scales to roughly half.
pub fn scrim(alpha_dark: f32) -> Hsla {
    scrim_for(current_appearance(), alpha_dark)
}

fn scrim_for(appearance: Appearance, alpha_dark: f32) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, alpha_dark),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.32 * (alpha_dark / SCRIM_ALPHA_DARK)),
    }
}

/// Recessed band behind a palette/picker header or footer strip.
///
/// A free function as well as a [`Theme`] field because the picker chrome that
/// paints it is built from context-free helpers; both resolve to the same value.
pub fn band() -> Hsla {
    band_for(current_appearance())
}

fn band_for(appearance: Appearance) -> Hsla {
    match appearance {
        Appearance::Dark => hsla(0.0, 0.0, 0.0, 0.16),
        // A recessed strip on white needs far less ink than on near-black; the
        // dark 16% would read as a bruise.
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.045),
    }
}

/// Selected-state glass treatment (tabs, session rows, space rows): a
/// TRANSLUCENT wash the vibrancy reads through — heavier flat washes blocked
/// the glass (user request). Dark: the 11% [`wash`]. Light: the tone-flipped
/// wash at 6% — 11% black read too dark over the bright frost (user report;
/// light also previously ran a near-opaque white chip, rejected the same
/// way). Same fill as [`Theme::glass_hover`] — the ring in
/// [`glass_selected_shadows`] is what distinguishes selection. Selection
/// *inside floating cards* is different — see [`card_selected_bg`].
pub fn glass_selected_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.11),
        Appearance::Light => wash(0.06),
    }
}

/// The user message bubble's plate: the same translucent wash family as
/// [`glass_selected_bg`], one step softer — at the selection weight the
/// bubble read too strong for settled content (user report), and an opaque
/// plate before that read as a solid slab over glass.
pub fn user_bubble_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.08),
        Appearance::Light => wash(0.04),
    }
}

/// Selected/keyboard-active treatment for rows and chips INSIDE a floating
/// card (menu rows, the picker rail, segmented chips). The card is already the
/// bright plane in light mode, so a white lift can't read there — selection is
/// the tone-flipped grey wash, at 6% (dark's 11% read too dark on the bright
/// plane, user report).
pub fn card_selected_bg() -> Hsla {
    match current_appearance() {
        Appearance::Dark => wash(0.11),
        Appearance::Light => wash(0.06),
    }
}

/// The selected chip's bright outline, as an INSET shadow: gpui paints inset
/// shadows ON TOP of the background, edges only — a border with zero layout
/// cost. Drop shadows are filled rects painted BEHIND the element, and behind
/// a 5% fill they showed straight through as an opaque dark plate with a
/// greyed ring (user report) — nothing may paint behind a glass chip.
///
/// Light pins the ring at a flat 7% black rather than the scaled hairline:
/// heavier rings (the [`INK_HAIRLINE_SCALE`]d value, then 12%) outlined every
/// selected chip in a dark box (user reports) — the ring should define the
/// chip the way dark's 9% white ring does, not frame it.
///
/// There is deliberately NO drop-shadow seat under the light chip. Three
/// recipes were tried (a tight 10% layer, a 6% contact + 5% ambient pair, a
/// lone 4% whisper) and every one failed on sight: layers sum into a grey rim
/// exactly where the chip meets the frost, gpui's small-radius blur reads
/// coarse on a bright field, and the tab strip is a scroll container that
/// clips its children vertically — any shadow escaping the chip gets cut off
/// mid-fade. The near-opaque fill plus the ring carry selection, exactly as
/// dark's wash plus ring does; the two appearances share one recipe now.
pub fn glass_selected_shadows() -> Vec<gpui::BoxShadow> {
    card_selected_shadows()
}

/// Selection outline for rows and chips INSIDE a floating card (menu rows,
/// the picker rail, segmented chips): the inset ring alone, in both
/// appearances. Card rows fill with a translucent wash
/// ([`card_selected_bg`]), and a drop shadow — a filled rect painted BEHIND
/// the element — shows straight through a translucent fill as a grey plate
/// (the same lesson [`glass_selected_shadows`] records for dark glass). The
/// card already carries the elevation shadow; selection inside it only needs
/// the edge.
pub fn card_selected_shadows() -> Vec<gpui::BoxShadow> {
    let color = match current_appearance() {
        Appearance::Dark => hairline(0.09),
        Appearance::Light => hsla(0.0, 0.0, 0.0, 0.07),
    };
    vec![gpui::BoxShadow {
        color,
        offset: gpui::point(gpui::px(0.0), gpui::px(0.0)),
        blur_radius: gpui::px(0.0),
        spread_radius: gpui::px(1.0),
        inset: true,
    }]
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ `#0d0d0d`)
/// — for surfaces matched against reference-screenshot samples.
pub fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to gpui Hsla.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
/// Reference: Björn Ottosson's OKLab definition (the same matrices CSS Color 4 uses).
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's Hsla convention).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

/// HSL (gpui convention, all 0..1) → sRGB components 0..1.
pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    if s <= f32::EPSILON {
        return [l, l, l];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0)]
}

/// WCAG 2.1 relative luminance of an opaque color.
pub fn relative_luminance(color: Hsla) -> f32 {
    let lin = |c: f32| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let [r, g, b] = hsl_to_rgb(color.h, color.s, color.l);
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG 2.1 contrast ratio between two opaque colors (1.0 … 21.0).
///
/// Used by the palette tests to prove each light token reproduces the contrast
/// its dark counterpart had, rather than merely looking plausible.
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Composite `fg` (which may be translucent) over an opaque `bg`, returning the
/// opaque result — the color the eye actually receives.
pub fn flatten(fg: Hsla, bg: Hsla) -> Hsla {
    let a = fg.a.clamp(0.0, 1.0);
    let [fr, fg_, fb] = hsl_to_rgb(fg.h, fg.s, fg.l);
    let [br, bg_, bb] = hsl_to_rgb(bg.h, bg.s, bg.l);
    let (h, s, l) = rgb_to_hsl(
        fr * a + br * (1.0 - a),
        fg_ * a + bg_ * (1.0 - a),
        fb * a + bb * (1.0 - a),
    );
    hsla(h, s, l, 1.0)
}

/// Linear per-component mix of two colors (paint helper for the gradient spinner).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    // Mix through hue naively — both spinner endpoints sit close enough on the
    // wheel that shortest-arc handling isn't needed for our palette.
    hsla(
        lerp(a.h, b.h),
        lerp(a.s, b.s),
        lerp(a.l, b.l),
        lerp(a.a, b.a),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_u8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn neutral_950_is_0a0a0a() {
        // oklch(0.145 0 0) is Tailwind neutral-950, zeron's app background.
        let rgb = srgb_u8(oklch_to_srgb(0.145, 0.0, 0.0));
        assert_eq!(rgb, [10, 10, 10]);
    }

    #[test]
    fn oklch_accents_match_reference() {
        // Reference values computed independently (CSS Color 4 matrices).
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.673, 0.182, 276.935)),
            [124, 134, 255]
        ); // indigo-400
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.704, 0.191, 22.216)),
            [255, 100, 103]
        ); // red-400
        assert_eq!(srgb_u8(oklch_to_srgb(0.828, 0.189, 84.429)), [255, 185, 0]);
        // amber-400
    }

    #[test]
    fn hsl_roundtrips_through_rgb() {
        for c in [
            Theme::dark().accent,
            Theme::dark().warning,
            Theme::light().accent,
            Theme::light().danger,
            neutral(0.556),
        ] {
            let [r, g, b] = hsl_to_rgb(c.h, c.s, c.l);
            let (h, s, l) = rgb_to_hsl(r, g, b);
            assert!((l - c.l).abs() < 1e-3, "lightness drift for {c:?}");
            assert!((s - c.s).abs() < 1e-3, "saturation drift for {c:?}");
            if c.s > 1e-3 {
                assert!((h - c.h).abs() < 1e-3, "hue drift for {c:?}");
            }
        }
    }

    #[test]
    fn contrast_ratio_hits_known_anchors() {
        let white = grey(0xff);
        let black = grey(0x00);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
        // Symmetric regardless of argument order.
        assert!((contrast_ratio(black, white) - contrast_ratio(white, black)).abs() < 1e-4);
    }

    /// The core claim of the light palette: it is *paired* to dark by contrast
    /// ratio, not mirrored by lightness. Each text token must land within 1.0 of
    /// its counterpart's ratio against its own background.
    #[test]
    fn text_contrast_is_paired_across_appearances() {
        let (d, l) = (Theme::dark(), Theme::light());
        for (name, dark_fg, light_fg) in [
            ("text", d.text, l.text),
            ("text_muted", d.text_muted, l.text_muted),
            ("text_faint", d.text_faint, l.text_faint),
        ] {
            let dr = contrast_ratio(dark_fg, d.bg);
            let lr = contrast_ratio(light_fg, l.bg);
            assert!(
                (dr - lr).abs() < 1.0,
                "{name}: dark {dr:.2}:1 vs light {lr:.2}:1 — not a matched pair"
            );
        }
    }

    /// Body and secondary text must clear WCAG AA (4.5:1) against **both** planes
    /// they can land on, in both appearances.
    ///
    /// `text_faint` is held to a lower floor on purpose. It is placeholder and
    /// disabled-control copy only, which WCAG 1.4.3 exempts, and the *existing
    /// dark palette* already measures ~4.2:1 there (neutral-500 on #060606). The
    /// light tone is matched to that inherited number rather than raised past it,
    /// so the two appearances stay siblings; raising the floor is a palette
    /// decision for both modes at once, not something light mode should do alone.
    #[test]
    fn text_tones_clear_wcag_aa() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, fg, floor) in [
                ("text", t.text, 4.5),
                ("text_muted", t.text_muted, 4.5),
                ("text_dim", t.text_dim, 4.5),
                ("text_faint", t.text_faint, 4.1),
            ] {
                let on_bg = contrast_ratio(fg, t.bg);
                let on_surface = contrast_ratio(fg, t.surface);
                assert!(
                    on_bg >= floor,
                    "{:?} {name} on bg is {on_bg:.2}:1, below {floor}",
                    t.appearance
                );
                assert!(
                    on_surface >= floor,
                    "{:?} {name} on surface is {on_surface:.2}:1, below {floor}",
                    t.appearance
                );
            }
        }
    }

    /// Accents are the tokens a naive invert gets most wrong: the dark theme's
    /// 400-step indigo/red land near 3:1 on white. The light palette drops to the
    /// 600 step at the same hue, which must clear AA for non-text UI (3:1) and,
    /// for the accent proper, body-text AA.
    #[test]
    fn accents_clear_contrast_on_their_background() {
        let l = Theme::light();
        assert!(
            contrast_ratio(l.accent, l.bg) >= 4.5,
            "light accent {:.2}:1",
            contrast_ratio(l.accent, l.bg)
        );
        assert!(
            contrast_ratio(l.danger, l.bg) >= 4.0,
            "light danger {:.2}:1",
            contrast_ratio(l.danger, l.bg)
        );
        for c in [l.warning, l.success, l.busy] {
            assert!(
                contrast_ratio(c, l.bg) >= 3.0,
                "light status color {:.2}:1 — below the 3:1 non-text floor",
                contrast_ratio(c, l.bg)
            );
        }
        // And the dark 400-step accents would NOT have cleared it — this is why
        // the light theme reassigns rather than reuses.
        let d = Theme::dark();
        assert!(
            contrast_ratio(d.warning, l.bg) < 3.0,
            "dark amber-400 unexpectedly passes on white; the invert-is-wrong \
             premise needs rechecking"
        );
    }

    /// Code is *text*, so syntax tones are held to the body-copy bar, not the
    /// 3:1 non-text floor. These are the tokens most likely to be picked by eye
    /// from a dark-theme screenshot and silently fail once the page turns white.
    #[test]
    fn code_and_syntax_tones_are_readable() {
        for t in [Theme::dark(), Theme::light()] {
            for (name, fg) in [
                ("code_text", t.code_text),
                ("syntax_keyword", t.syntax.keyword),
                ("syntax_string", t.syntax.string),
                ("syntax_number", t.syntax.number),
            ] {
                let r = contrast_ratio(fg, t.bg);
                assert!(r >= 4.5, "{:?} {name} is {r:.2}:1 on bg", t.appearance);
            }
            // Diff tints mark whole rows; the 3:1 non-text floor applies.
            for (name, fg) in [("diff_add", t.diff_add), ("diff_del", t.diff_del)] {
                let r = contrast_ratio(fg, t.bg);
                assert!(r >= 3.0, "{:?} {name} is {r:.2}:1 on bg", t.appearance);
            }
        }
    }

    #[test]
    fn syntax_palette_resolves_every_kind_on_code_and_diff_backgrounds() {
        let kinds = [
            HighlightKind::Comment,
            HighlightKind::Keyword,
            HighlightKind::String,
            HighlightKind::StringSpecial,
            HighlightKind::Escape,
            HighlightKind::Number,
            HighlightKind::Boolean,
            HighlightKind::Type,
            HighlightKind::TypeBuiltin,
            HighlightKind::Constructor,
            HighlightKind::Function,
            HighlightKind::FunctionBuiltin,
            HighlightKind::Macro,
            HighlightKind::Property,
            HighlightKind::Constant,
            HighlightKind::Variable,
            HighlightKind::VariableSpecial,
            HighlightKind::Parameter,
            HighlightKind::Operator,
            HighlightKind::Punctuation,
            HighlightKind::Tag,
            HighlightKind::Attribute,
            HighlightKind::Label,
            HighlightKind::Embedded,
            HighlightKind::Invalid,
        ];
        for theme in [Theme::dark(), Theme::light()] {
            let add_bg = flatten(theme.diff_add.opacity(0.055), theme.bg);
            let del_bg = flatten(theme.diff_del.opacity(0.055), theme.bg);
            for kind in kinds {
                let color = theme.syntax.color(kind);
                let floor = if matches!(
                    kind,
                    HighlightKind::Comment
                        | HighlightKind::Operator
                        | HighlightKind::Punctuation
                        | HighlightKind::Embedded
                ) {
                    3.0
                } else {
                    4.5
                };
                for (name, background) in [("code", theme.bg), ("add", add_bg), ("del", del_bg)] {
                    let ratio = contrast_ratio(color, background);
                    assert!(
                        ratio >= floor,
                        "{:?} {kind:?} is {ratio:.2}:1 on {name}",
                        theme.appearance
                    );
                }
            }
        }
    }

    /// The caret is a 2px bar, so the 3:1 non-text floor applies — but it is the
    /// one element the user is actively hunting for, and the dark-mode blue is
    /// far too light to survive on white unchanged.
    #[test]
    fn caret_is_findable_on_its_background() {
        for t in [Theme::dark(), Theme::light()] {
            let r = contrast_ratio(t.caret, t.bg);
            assert!(r >= 3.0, "{:?} caret is {r:.2}:1 on bg", t.appearance);
        }
    }

    /// Solid (primary button) plates must carry their label at AA in both modes.
    ///
    /// The accent plate is held to 4.0 rather than 4.5: dark mode's indigo-500
    /// fill — inherited unchanged from the original palette — measures 4.38:1
    /// under white, which clears WCAG AA for the medium-weight 14px labels these
    /// buttons use (large-text AA is 3:1) but not body copy. Light mode's
    /// indigo-600 clears the stricter bar with room to spare.
    #[test]
    fn solid_button_is_legible_in_both_appearances() {
        for t in [Theme::dark(), Theme::light()] {
            let r = contrast_ratio(t.on_solid, t.solid);
            assert!(r >= 7.0, "{:?} solid button {r:.2}:1", t.appearance);
            let a = contrast_ratio(t.on_accent, t.accent_strong);
            assert!(a >= 4.0, "{:?} accent button {a:.2}:1", t.appearance);
        }
    }

    /// Surfaces must stay *distinguishable*, but the direction differs: dark
    /// stacks upward in lightness, light puts the content plane on top and lets
    /// chrome recede. Asserting separation (not a fixed order) is the point.
    #[test]
    fn surfaces_are_separated_in_both_appearances() {
        let d = Theme::dark();
        assert!(d.bg.l < d.surface.l, "dark: chrome sits above content");
        assert!(d.surface.l < d.surface_raised.l, "dark: raised is lighter");

        let l = Theme::light();
        assert!(
            l.surface.l < l.bg.l,
            "light: chrome recedes *below* the content plane"
        );
        assert!(
            (l.bg.l - l.surface.l) > 0.015,
            "light: sidebar must be visibly separated from the panel"
        );
        // Raised surfaces are white in light mode; separation comes from the
        // border, so the border must be strong enough to carry it alone.
        assert!(contrast_ratio(flatten(l.border, l.bg), l.bg) > 1.15);
    }

    /// The dark elevation steps are small but deliberate, and each plane must
    /// stay strictly above the one below. This test exists because collapsing the
    /// ladder onto a single `surface_raised` is the tempting simplification — and
    /// it visibly lifts every popover off its plane.
    #[test]
    fn dark_elevation_ladder_is_strictly_ordered() {
        let d = Theme::dark();
        let ladder = [
            ("bg", d.bg),
            ("surface_card", d.surface_card),
            ("surface_dialog", d.surface_dialog),
            ("surface_overlay", d.surface_overlay),
            ("surface_raised", d.surface_raised),
        ];
        for pair in ladder.windows(2) {
            let ((lower, lo), (upper, hi)) = (pair[0], pair[1]);
            assert!(
                lo.l < hi.l,
                "dark: {upper} ({:.4}) must sit above {lower} ({:.4})",
                hi.l,
                lo.l
            );
        }
    }

    /// Light mode flattens the ladder onto white on purpose — separation comes
    /// from border and shadow. Assert that explicitly so nobody "fixes" it by
    /// reintroducing lightness steps that would tint popovers grey.
    #[test]
    fn light_elevation_is_flat_white_and_leans_on_borders() {
        let l = Theme::light();
        for (name, c) in [
            ("surface_card", l.surface_card),
            ("surface_dialog", l.surface_dialog),
            ("surface_overlay", l.surface_overlay),
        ] {
            assert_eq!(c.l, 1.0, "light {name} should be white");
        }
        // With no lightness step available, the border is the only separator —
        // it has to actually register against the plane behind it.
        assert!(contrast_ratio(flatten(l.border, l.bg), l.bg) > 1.15);
    }

    /// `surface_raised` is the *bare plate* tone — user message bubbles, the
    /// jump-to-bottom pill. Unlike the popover ladder it gets no border and no
    /// shadow, so lightness is the only thing separating it from the panel. It
    /// was white in light mode once, which made the user's own messages
    /// indistinguishable from the page.
    #[test]
    fn bare_plates_are_visible_against_their_panel() {
        for t in [Theme::dark(), Theme::light()] {
            let delta = (t.surface_raised.l - t.bg.l).abs();
            assert!(
                delta > 0.03,
                "{:?} surface_raised ({:.3}) is only {delta:.3} from bg ({:.3}) — \
                 a plate with no border needs lightness to read",
                t.appearance,
                t.surface_raised.l,
                t.bg.l
            );
            // And hovering it has to go somewhere visible too.
            let hover_delta = (t.surface_raised_hover.l - t.surface_raised.l).abs();
            assert!(
                hover_delta > 0.02,
                "{:?} raised-plate hover moves only {hover_delta:.3}",
                t.appearance
            );
        }
    }

    /// Monochrome discipline: neutrals carry no saturation in either appearance.
    #[test]
    fn neutrals_are_achromatic() {
        for t in [Theme::dark(), Theme::light()] {
            for c in [
                t.bg,
                t.surface,
                t.surface_raised,
                t.text,
                t.text_muted,
                t.text_faint,
                t.solid,
                t.on_solid,
            ] {
                assert_eq!(c.s, 0.0, "{:?} neutral has chroma", t.appearance);
                assert_eq!(c.a, 1.0, "{:?} neutral is translucent", t.appearance);
            }
        }
    }

    #[test]
    fn hairlines_and_washes_flip_tone_with_appearance() {
        let _guard = lock_appearance();
        set_current_appearance(Appearance::Dark);
        assert_eq!(hairline(0.1).l, 1.0, "dark hairlines are white");
        assert_eq!(ink(0.1).l, 1.0, "dark fills are white");
        assert_eq!(ink(0.1).a, 0.1, "dark alphas pass through untouched");
        assert_eq!(wash(0.14).l, 0.92, "dark washes are soft-white");

        set_current_appearance(Appearance::Light);
        assert_eq!(hairline(0.1).l, 0.0, "light hairlines are black");
        assert_eq!(ink(0.1).l, 0.0, "light fills are black");
        assert_eq!(wash(0.14).l, 0.10, "light washes are soft-black");
        // Fills keep their alpha; only hairlines are scaled.
        assert_eq!(ink(0.10).a, 0.10, "light fills keep their alpha");
        assert!(hairline(0.10).a > 0.10, "light hairlines strengthen");
        assert!(hairline(0.60).a <= 0.5, "hairline alpha is capped");

        set_current_appearance(Appearance::Dark);
    }

    /// A hover wash has to actually be *visible* against the surface it lands on,
    /// in both appearances — the failure mode of a halved light alpha.
    #[test]
    fn hover_wash_is_visible_on_its_surface() {
        let _guard = lock_appearance();
        for (appearance, theme) in [
            (Appearance::Dark, Theme::dark()),
            (Appearance::Light, Theme::light()),
        ] {
            set_current_appearance(appearance);
            let hovered = flatten(wash(0.14), theme.surface);
            let delta = (hovered.l - theme.surface.l).abs();
            assert!(
                delta > 0.02,
                "{appearance:?} hover wash shifts lightness by only {delta:.4}"
            );
        }
        set_current_appearance(Appearance::Dark);
    }

    /// The regression that shipped: subtle fills are quoted at very low alphas
    /// (`ink(0.03)` is the composer plate, `ink(0.05)` a key cap), and scaling
    /// those down for light mode erased them — the composer rendered as bare text
    /// on white. Assert the faintest fill we actually use still moves the surface
    /// it lands on, in *both* appearances.
    #[test]
    fn faintest_fills_survive_in_both_appearances() {
        let _guard = lock_appearance();
        for (appearance, theme) in [
            (Appearance::Dark, Theme::dark()),
            (Appearance::Light, Theme::light()),
        ] {
            set_current_appearance(appearance);
            for alpha in [0.03, 0.05] {
                let plate = flatten(ink(alpha), theme.bg);
                let delta = (plate.l - theme.bg.l).abs();
                assert!(
                    delta >= 0.02,
                    "{appearance:?} ink({alpha}) shifts its background by only \
                     {delta:.4} — the fill is invisible"
                );
            }
        }
        set_current_appearance(Appearance::Dark);
    }

    /// Both appearances are glass-forward on macOS. Light frost runs heavier
    /// than dark's (a light tint controls the blur less), and floating cards
    /// step their tint coverage up in light so menu text stays on a
    /// known-enough background — assert both relationships so the frost and
    /// the overlay can't drift apart.
    #[test]
    fn both_appearances_stay_frosted_and_light_runs_heavier() {
        if Theme::GLASS_ALPHA < 1.0 {
            let (dark, light) = (Theme::dark(), Theme::light());
            assert!(dark.glass().a < 1.0, "dark keeps its translucent frost");
            assert!(light.glass().a < 1.0, "light is glass-forward like dark");
            assert!(
                light.glass().a > dark.glass().a - f32::EPSILON,
                "a light tint dominates the blur less, so it must not run looser than dark"
            );
            assert!(
                light.glass_overlay().a > dark.glass_overlay().a,
                "light floating cards need more coverage over blur for legible rows"
            );
        } else {
            assert_eq!(Theme::light().glass().a, 1.0);
            assert_eq!(Theme::dark().glass().a, 1.0);
        }
    }

    /// An input plate has to read as *lifted* in both appearances. Dark does that
    /// with a faint white wash; the literal light translation is a faint black
    /// wash, which reads as a dent instead — so light lifts with white plus its
    /// border. Assert the plate is never darker than the panel it sits on.
    #[test]
    fn input_plate_never_reads_as_recessed() {
        for t in [Theme::dark(), Theme::light()] {
            let plate = flatten(t.input_bg, t.bg);
            assert!(
                plate.l >= t.bg.l,
                "{:?} input plate ({:.3}) is darker than its panel ({:.3}) — \
                 that reads as recessed, not raised",
                t.appearance,
                plate.l,
                t.bg.l
            );
        }
    }

    /// Card rows fill with translucent washes, and a drop shadow behind a
    /// translucent fill shows through as a grey plate — selection inside a
    /// floating card must be edge-only. This regressed once: light menu rows
    /// borrowed the glass-chip recipe, drop shadow included.
    #[test]
    fn card_selection_paints_nothing_behind_its_row() {
        let _guard = lock_appearance();
        for appearance in [Appearance::Dark, Appearance::Light] {
            set_current_appearance(appearance);
            for shadow in card_selected_shadows() {
                assert!(
                    shadow.inset,
                    "{appearance:?}: card selection may only paint inset edges"
                );
            }
        }
        set_current_appearance(Appearance::Dark);
    }

    /// Glass selection is edge-only in BOTH appearances — no drop-shadow seat.
    /// Every light seat tried (10% tight, 6%+5% pair, lone 4%) read as a grey
    /// rim or a coarse smudge, and the tab strip clips escaping shadows
    /// vertically (user reports). The ring must also stay subtle enough to
    /// define the chip rather than frame it.
    #[test]
    fn glass_selection_is_edge_only_and_subtle() {
        let _guard = lock_appearance();
        for appearance in [Appearance::Dark, Appearance::Light] {
            set_current_appearance(appearance);
            let shadows = glass_selected_shadows();
            assert!(
                shadows.iter().all(|s| s.inset),
                "{appearance:?}: glass selection may only paint inset edges"
            );
            let ring = shadows.iter().find(|s| s.inset).expect("selection ring");
            assert!(
                ring.color.a <= 0.09,
                "{appearance:?}: ring at {:.2} alpha frames the chip instead of defining it",
                ring.color.a
            );
        }
        set_current_appearance(Appearance::Dark);
    }

    #[test]
    fn appearance_mirror_tracks_installed_theme() {
        let _guard = lock_appearance();
        set_current_appearance(Appearance::Light);
        assert_eq!(current_appearance(), Appearance::Light);
        set_current_appearance(Appearance::Dark);
        assert_eq!(current_appearance(), Appearance::Dark);
    }

    #[test]
    fn window_appearance_maps_onto_ours() {
        use gpui::WindowAppearance as W;
        assert_eq!(Appearance::from_window(W::Light), Appearance::Light);
        assert_eq!(Appearance::from_window(W::VibrantLight), Appearance::Light);
        assert_eq!(Appearance::from_window(W::Dark), Appearance::Dark);
        assert_eq!(Appearance::from_window(W::VibrantDark), Appearance::Dark);
    }

    #[test]
    fn scrim_is_black_but_lighter_in_light_mode() {
        let (d, l) = (Theme::dark(), Theme::light());
        assert_eq!(d.scrim().l, 0.0);
        assert_eq!(l.scrim().l, 0.0);
        assert!(l.scrim().a < d.scrim().a);
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = hsla(0.0, 0.0, 0.0, 1.0);
        let b = hsla(0.5, 1.0, 1.0, 0.0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let mid = mix(a, b, 0.5);
        assert!((mid.l - 0.5).abs() < 1e-6 && (mid.a - 0.5).abs() < 1e-6);
        // Out-of-range t clamps.
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn layout_numbers_match_zeron() {
        assert_eq!(Theme::HEADER_HEIGHT, 44.0); // h-11
        assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0); // h-6
        assert_eq!(Theme::BUBBLE_RADIUS, 16.0);
    }

    #[test]
    fn theme_colors_round_trip_through_json() {
        // Hex quantizes to 1/255 and HSL↔RGB conversion rounds, so compare
        // with a per-component tolerance instead of exact equality.
        fn assert_close(label: &str, a: Hsla, b: Hsla) {
            let eps = 0.005;
            assert!((a.h - b.h).abs() < eps, "{label}: hue {} vs {}", a.h, b.h);
            assert!((a.s - b.s).abs() < eps, "{label}: sat {} vs {}", a.s, b.s);
            assert!(
                (a.l - b.l).abs() < eps,
                "{label}: lightness {} vs {}",
                a.l,
                b.l
            );
            assert!((a.a - b.a).abs() < eps, "{label}: alpha {} vs {}", a.a, b.a);
        }

        for stock in [ThemeColors::stock_dark(), ThemeColors::stock_light()] {
            let json = serde_json::to_string(&stock).unwrap();
            let parsed: ThemeColors = serde_json::from_str(&json).unwrap();
            assert_close("bg", parsed.bg, stock.bg);
            assert_close("surface", parsed.surface, stock.surface);
            assert_close("accent", parsed.accent, stock.accent);
            assert_close("element_hover", parsed.element_hover, stock.element_hover);
            assert_close(
                "syntax.keyword",
                parsed.syntax.keyword,
                stock.syntax.keyword,
            );
            assert_eq!(parsed.glass_tint, stock.glass_tint);
            assert_eq!(parsed.glass_overlay_tint, stock.glass_overlay_tint);
            // Hex, not floats — the file format humans read and edit.
            assert!(json.contains("\"bg\":\"#"), "{json}");
        }
    }

    #[test]
    fn translucent_tokens_serialize_with_alpha_hex() {
        // Dark input_bg is a 3% white wash — must not round-trip as opaque.
        let json = serde_json::to_string(&ThemeColors::stock_dark()).unwrap();
        assert!(
            json.contains("#FFFFFF08") || json.contains("#ffffff08"),
            "{json}"
        );
    }

    #[test]
    fn missing_fields_and_unknown_ids_fall_back_to_stock() {
        use crate::themes;

        // Omitted glass overrides parse back as None (derive, not pin).
        let minimal = r##"{"bg":"#060606","surface":"#0d0d0d","surfaceRaised":"#3b3b3b","surfaceCard":"#0e0e0e","surfaceDialog":"#101010","surfaceOverlay":"#161616","elementHover":"#ebebeb1c","elementActive":"#ebebeb29","border":"#ffffff14","borderStrong":"#ffffff24","text":"#ebebeb","textMuted":"#b4b4b4","textFaint":"#8e8e8e","textDim":"#989898","solid":"#ebebeb","onSolid":"#0e0e0e","accent":"#7c86ff","accentStrong":"#5c6aff","onAccent":"#fbfbfb","danger":"#ff6467","dangerMuted":"#ffa199","warning":"#ffb900","warningMuted":"#ffe599","success":"#34d399","busy":"#f472b6","successMuted":"#6ee7b7","surfaceRaisedHover":"#4a4a4a","band":"#00000029","inputBg":"#ffffff08","selection":"#59b2f259","cursor":"#ffffff59","caret":"#b3d4fc","dangerStrong":"#e5484d","codeText":"#c4b5fd","codeWash":"#a78bfa1f","syntax":{"comment":"#999999","keyword":"#7c82d9","string":"#35c493","stringSpecial":"#e08cc9","escape":"#e08cc9","number":"#ffc53d","boolean":"#ffc53d","typeName":"#ffc53d","typeBuiltin":"#35c493","constructor":"#ffc53d","function":"#7c82d9","functionBuiltin":"#e08cc9","macroName":"#e08cc9","property":"#ffc53d","constant":"#35c493","variable":"#ebebeb","variableSpecial":"#e08cc9","parameter":"#ebebeb","operator":"#ebebeb","punctuation":"#ebebeb","tag":"#e08cc9","attribute":"#ffc53d","label":"#ffc53d","invalid":"#ff6467"},"diffAdd":"#34d399","diffDel":"#ff6467","diffHunkBg":"#8fa4e60d"}"##;
        let parsed: ThemeColors = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.glass_tint, None);
        assert_eq!(parsed.glass_overlay_tint, None);

        // Unknown id and appearance mismatch both degrade to stock.
        assert_eq!(
            themes::resolve(Some("nope"), Appearance::Dark).id,
            themes::STOCK_DARK
        );
        assert_eq!(
            themes::resolve(Some(themes::STOCK_LIGHT), Appearance::Dark).id,
            themes::STOCK_DARK
        );
        assert_eq!(
            themes::resolve(None, Appearance::Light).id,
            themes::STOCK_LIGHT
        );
    }

    #[test]
    fn built_in_glass_pins_sampled_tints_custom_derives_from_surface() {
        if Theme::GLASS_ALPHA >= 1.0 {
            // Opaque platform: frost is off; glass() IS the surface either way.
            assert_eq!(Theme::dark().glass(), Theme::dark().surface);
            return;
        }
        // Built-ins pin their sampled frost greys (pixel-identical to the
        // pre-theme build).
        assert_eq!(Theme::dark().glass(), grey(8).opacity(Theme::GLASS_ALPHA));
        assert_eq!(
            Theme::light().glass(),
            grey(0xfa).opacity(Theme::GLASS_ALPHA_LIGHT)
        );

        // A palette without an override derives its frost from its own surface,
        // so custom themes get coherent glass for free.
        let mut custom = ThemeColors::stock_dark();
        custom.glass_tint = None;
        let theme = Theme::from_colors(custom, "x", "X", Appearance::Dark);
        assert_eq!(theme.glass(), theme.surface.opacity(Theme::GLASS_ALPHA));
    }

    #[test]
    fn glass_alpha_controls_frost_opacity() {
        // Opaque platform: glass() is always the surface regardless of alpha.
        if Theme::GLASS_ALPHA >= 1.0 {
            assert_eq!(Theme::dark().glass(), Theme::dark().surface);
            return;
        }

        let mut theme = Theme::dark();
        theme.glass_alpha = 1.0; // Off
        assert_eq!(theme.glass(), theme.surface);
        assert!(!theme.is_glass());

        theme.glass_alpha = 0.50;
        assert_eq!(theme.glass(), theme.glass_tint.unwrap().opacity(0.50));
        assert!(theme.is_glass());
    }

    #[test]
    fn glass_overlay_override_wins_recipe_stays_default() {
        // Default (None): the per-appearance recipe — surface_overlay at 85%.
        let stock = Theme::light();
        assert_eq!(stock.glass_overlay(), stock.surface_overlay.opacity(0.85));

        let mut colors = ThemeColors::stock_light();
        let tint = hsla(0.55, 0.3, 0.4, 0.5);
        colors.glass_overlay_tint = Some(tint);
        let theme = Theme::from_colors(colors, "x", "X", Appearance::Light);
        assert_eq!(theme.glass_overlay(), tint);
    }
}
