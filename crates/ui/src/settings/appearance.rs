//! Settings → Appearance: pick between following the system and pinning light or
//! dark.
//!
//! Uses [`widgets::option_card_row`] — a preview-card picker, because the choice
//! is a *look*, and a miniature of the result says more than a sentence about it.
//! The control itself is theme-agnostic; only the previews below know what a
//! theme is.
//!
//! Stateless. The choice lives in the [`crate::appearance`] globals, and
//! `set_mode` repaints every window, so this page has nothing of its own to hold.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Hsla, IntoElement, Render, SharedString, Window,
};

use crate::appearance::{self, AppearanceMode};
use crate::settings::widgets;
use crate::theme::{Appearance, Theme};

pub struct AppearancePage;

impl AppearancePage {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

/// One placeholder bar in the miniature, width given as a fraction of its
/// container.
///
/// Relative rather than fixed px because the System card renders this same
/// miniature into *half* a card. Fixed widths were wider than the squeezed
/// content pane and spilled out over the card edge.
fn bar(fraction: f32, tone: Hsla) -> gpui::Div {
    div()
        .h(px(5.0))
        .w(gpui::relative(fraction))
        .rounded(px(3.0))
        .bg(tone)
}

/// Which corners a miniature rounds — the split card needs each half to round
/// only its outer side so the two meet flush down the middle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corners {
    All,
    Left,
    Right,
}

/// A miniature of the app in `theme`: sidebar strip, inset content card, a few
/// placeholder lines. Built from the theme's own tokens rather than fixed
/// swatches, so the previews stay honest if the palette is retuned.
///
/// Rounds itself: the card frame cannot do it for us (see
/// [`widgets::OPTION_CARD_RADIUS`]). Only this root paints a background that
/// reaches the corners — the sidebar strip is transparent and the content card is
/// inset — so rounding here is enough.
fn miniature(theme: &Theme, corners: Corners) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div().size_full().flex().flex_row().bg(theme.surface);
    let root = match corners {
        Corners::All => root.rounded(r),
        Corners::Left => root.rounded_tl(r).rounded_bl(r),
        Corners::Right => root.rounded_tr(r).rounded_br(r),
    };
    root.child(
        // Sidebar strip.
        div()
            .w(px(44.0))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(8.0))
            .pt(px(14.0))
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        // Inset content card — the same rounded plate the real shell floats.
        div()
            .flex_1()
            .min_w_0()
            .my(px(8.0))
            .mr(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(10.0))
            .child(bar(0.62, strong))
            .child(bar(0.88, line))
            .child(bar(0.76, line))
            .child(bar(0.52, line)),
    )
    .into_any_element()
}

/// The System card: light on the left, dark on the right. Each half is a
/// complete miniature clipped to its side, which is what makes the card read as
/// "whichever one the system is on".
fn miniature_split() -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::light(), Corners::Left)),
        )
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::dark(), Corners::Right)),
        )
        .into_any_element()
}

/// Frost presets: label → global glass opacity. `Off` paints the chrome opaque
/// (the platform default outside macOS).
const FROST_PRESETS: &[(&str, f32)] = &[
    ("Off", 1.0),
    ("Standard", 0.80),
    ("Clear", 0.60),
    ("Frosted", 0.40),
];

/// A miniature showing how frosted the chrome is at `alpha`: a vivid wallpaper
/// stands in for the desktop, the sidebar strip paints the frost tint at
/// `alpha`, and the content card stays opaque so the difference reads.
fn glass_miniature(theme: &Theme, alpha: f32) -> AnyElement {
    let tint = theme.glass_tint.unwrap_or(theme.surface);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let wallpaper: Hsla = gpui::rgb(0x6a5acd).into();
    div()
        .size_full()
        .rounded(r)
        .overflow_hidden()
        .flex()
        .flex_row()
        .bg(wallpaper)
        .child(
            // Sidebar strip — the frosted chrome.
            div().w(px(44.0)).h_full().flex_none().bg(if alpha >= 1.0 {
                theme.surface
            } else {
                tint.opacity(alpha)
            }),
        )
        .child(
            // Inset opaque content card.
            div()
                .flex_1()
                .h_full()
                .m(px(8.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.bg),
        )
        .into_any_element()
}

/// The preview graphic for a mode.
///
/// The one place `Theme::light()`/`Theme::dark()` are legitimately built outside
/// the installed global: a preview has to show the palette you are *not* using.
fn preview(mode: AppearanceMode) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(),
        AppearanceMode::Light => miniature(&Theme::light(), Corners::All),
        AppearanceMode::Dark => miniature(&Theme::dark(), Corners::All),
    }
}

/// The selectable themes for one appearance, as preview cards. Clicking installs
/// that theme for the appearance (and repaints via [`appearance::set_theme`]).
fn theme_cards(
    appearance: Appearance,
    selected_id: &str,
    page_theme: &Theme,
    cx: &mut Context<AppearancePage>,
) -> Vec<gpui::Stateful<gpui::Div>> {
    crate::themes::built_ins_for(appearance)
        .into_iter()
        .map(|entry| {
            let id = entry.id;
            let preview_theme =
                Theme::from_colors(entry.colors.clone(), entry.id, entry.name, entry.appearance);
            widgets::option_card(
                page_theme,
                entry.name,
                entry.id == selected_id,
                miniature(&preview_theme, Corners::All),
            )
            .id(SharedString::from(format!("theme-{id}")))
            .on_click(cx.listener(move |_, _, _, cx| {
                appearance::set_theme(id, appearance, cx);
                cx.notify();
            }))
        })
        .collect()
}

/// Helper copy under the picker.
fn helper(mode: AppearanceMode, system: Appearance) -> SharedString {
    match mode {
        // Naming the resolved appearance makes "System" concrete — otherwise the
        // card says nothing about what you actually get right now.
        AppearanceMode::System => {
            let resolved = if system.is_dark() { "dark" } else { "light" };
            format!(
                "Following the system appearance — currently {resolved}. Zeron switches with \
                 macOS, including scheduled changes."
            )
            .into()
        }
        AppearanceMode::Light => "Always light, whatever the system is set to.".into(),
        AppearanceMode::Dark => "Always dark, whatever the system is set to.".into(),
    }
}

impl Render for AppearancePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let current = appearance::mode(cx);
        let state = cx.try_global::<appearance::AppearanceState>();
        let system = state.map(|state| state.system).unwrap_or_default();
        let dark_theme = state.and_then(|state| state.dark_theme.clone());
        let light_theme = state.and_then(|state| state.light_theme.clone());
        let dark_selected = crate::themes::resolve(dark_theme.as_deref(), Appearance::Dark).id;
        let light_selected = crate::themes::resolve(light_theme.as_deref(), Appearance::Light).id;
        let frost = state
            .map(|state| state.frost_alpha)
            .unwrap_or_else(crate::settings::default_frost_alpha);

        let cards = AppearanceMode::ALL.into_iter().map(|mode| {
            widgets::option_card(&theme, mode.label(), mode == current, preview(mode))
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
        });

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Appearance", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "How zeron picks between light and dark. This setting stays on this \
                             device.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Theme"))
                            .child(widgets::option_card_row().children(cards)),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .line_height(px(18.0))
                            .child(helper(current, system)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Dark theme"))
                            .child(widgets::option_card_row().children(theme_cards(
                                Appearance::Dark,
                                dark_selected,
                                &theme,
                                cx,
                            ))),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Light theme"))
                            .child(widgets::option_card_row().children(theme_cards(
                                Appearance::Light,
                                light_selected,
                                &theme,
                                cx,
                            ))),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Frost"))
                            .child(
                                widgets::option_card_row().children(FROST_PRESETS.iter().map(
                                    |(label, alpha)| {
                                        let label = *label;
                                        let alpha = *alpha;
                                        let selected = (frost - alpha).abs() < 0.001;
                                        widgets::option_card(
                                            &theme,
                                            label,
                                            selected,
                                            glass_miniature(&theme, alpha),
                                        )
                                        .id(SharedString::from(format!("frost-{label}")))
                                        .on_click(
                                            cx.listener(move |_, _, _, cx| {
                                                appearance::set_frost(alpha, cx);
                                                cx.notify();
                                            }),
                                        )
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .line_height(px(18.0))
                            .child(
                                "How much of the desktop shows through the sidebar chrome. macOS \
                                 only.",
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_card() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn system_helper_names_the_resolved_appearance() {
        let dark = helper(AppearanceMode::System, Appearance::Dark);
        let light = helper(AppearanceMode::System, Appearance::Light);
        assert!(dark.contains("currently dark"), "got {dark}");
        assert!(light.contains("currently light"), "got {light}");
    }

    /// The pinned modes must not claim to follow anything — that copy is the only
    /// thing telling the user the system setting is being ignored.
    #[test]
    fn pinned_helpers_do_not_mention_following() {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            for system in [Appearance::Light, Appearance::Dark] {
                let copy = helper(mode, system).to_lowercase();
                assert!(!copy.contains("following"), "{mode:?}: {copy}");
                assert!(copy.contains("whatever the system"), "{mode:?}: {copy}");
            }
        }
    }

    /// The previews must differ from each other, or the picker is decoration.
    /// Comparing the tones they are built from is the closest we can get without
    /// a renderer.
    #[test]
    fn light_and_dark_previews_draw_from_different_palettes() {
        let (l, d) = (Theme::light(), Theme::dark());
        assert_ne!(l.surface.l, d.surface.l);
        assert_ne!(l.bg.l, d.bg.l);
    }
}
