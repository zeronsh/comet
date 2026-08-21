//! Settings → Appearance: choose the palette and interface typography.
//!
use gpui::{
    AnyElement, Context, FocusHandle, Hsla, IntoElement, KeyDownEvent, Render, SharedString,
    Window, div, prelude::*, px,
};

use crate::appearance::{self, AppearanceMode};
use crate::icons::{self, icon};
use crate::popover;
use crate::settings::widgets;
use crate::theme::{Appearance, Theme};
use crate::typography::{self, FontAvailability, UiFontFamily, UiFontSize};

pub struct AppearancePage {
    selected_font: UiFontFamily,
    selected_size: UiFontSize,
    font_focus: FocusHandle,
    size_focus: FocusHandle,
    font_menu: popover::Popup<()>,
    size_menu: popover::Popup<()>,
    font_menu_dismissed_at: Option<std::time::Instant>,
    size_menu_dismissed_at: Option<std::time::Instant>,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            selected_font: typography::effective(cx),
            selected_size: typography::font_size(cx),
            font_focus: cx.focus_handle(),
            size_focus: cx.focus_handle(),
            font_menu: popover::Popup::default(),
            size_menu: popover::Popup::default(),
            font_menu_dismissed_at: None,
            size_menu_dismissed_at: None,
        }
    }

    fn commit_font(&mut self, cx: &mut Context<Self>) {
        if typography::is_available(&self.selected_font, cx) {
            typography::set_family(self.selected_font.clone(), cx);
            self.selected_font = typography::effective(cx);
            self.close_font_menu(cx);
            cx.notify();
        }
    }

    fn commit_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        typography::set_font_size(self.selected_size, window, cx);
        self.selected_size = typography::font_size(cx);
        self.close_size_menu(cx);
        cx.notify();
    }

    fn close_font_menu(&mut self, cx: &mut Context<Self>) {
        if self.font_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.font_menu);
        }
    }

    fn close_size_menu(&mut self, cx: &mut Context<Self>) {
        if self.size_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.size_menu);
        }
    }

    fn dismiss_font_menu(&mut self, cx: &mut Context<Self>) {
        self.font_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_font_menu(cx);
    }

    fn dismiss_size_menu(&mut self, cx: &mut Context<Self>) {
        self.size_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_size_menu(cx);
    }

    fn toggle_font_menu(&mut self, cx: &mut Context<Self>) {
        self.close_size_menu(cx);
        let just_dismissed = self
            .font_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.font_menu.is_open() {
            self.close_font_menu(cx);
        } else if !just_dismissed {
            self.selected_font = typography::effective(cx);
            self.font_menu.open(());
        }
        cx.notify();
    }

    fn toggle_size_menu(&mut self, cx: &mut Context<Self>) {
        self.close_font_menu(cx);
        let just_dismissed = self
            .size_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.size_menu.is_open() {
            self.close_size_menu(cx);
        } else if !just_dismissed {
            self.selected_size = typography::font_size(cx);
            self.size_menu.open(());
        }
        cx.notify();
    }

    fn on_font_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let availability = typography::availability(cx);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, -1, &availability);
                cx.notify();
            }
            "down" | "right" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, 1, &availability);
                cx.notify();
            }
            "home" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = first_available(&availability);
                cx.notify();
            }
            "end" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = last_available(&availability);
                cx.notify();
            }
            "enter" | "space" => {
                if self.font_menu.is_open() {
                    self.commit_font(cx);
                } else {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
            }
            "escape" => {
                self.selected_font = typography::effective(cx);
                self.close_font_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn on_size_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = UiFontSize::ALL
            .iter()
            .position(|size| *size == self.selected_size)
            .unwrap_or(4);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[current.saturating_sub(1)];
                cx.notify();
            }
            "down" | "right" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[(current + 1).min(UiFontSize::ALL.len() - 1)];
                cx.notify();
            }
            "home" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[0];
                cx.notify();
            }
            "end" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[UiFontSize::ALL.len() - 1];
                cx.notify();
            }
            "enter" | "space" => {
                if self.size_menu.is_open() {
                    self.commit_size(window, cx);
                } else {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
            }
            "escape" => {
                self.selected_size = typography::font_size(cx);
                self.close_size_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }
}

fn step_font(
    current: &UiFontFamily,
    delta: isize,
    availability: &FontAvailability,
) -> UiFontFamily {
    let choices = availability.choices();
    let current = choices
        .iter()
        .position(|family| family == current)
        .unwrap_or_default() as isize;
    let mut ix = current + delta.signum();
    while (0..choices.len() as isize).contains(&ix) {
        let candidate = &choices[ix as usize];
        if availability.is_available(candidate) {
            return candidate.clone();
        }
        ix += delta.signum();
    }
    choices[current as usize].clone()
}

fn first_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
}

fn last_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .rev()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
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
        let system = cx
            .try_global::<appearance::AppearanceState>()
            .map(|state| state.system)
            .unwrap_or_default();
        let effective_font = typography::effective(cx);
        let requested_font = typography::requested(cx);
        let availability = typography::availability(cx);
        let fixed = theme.font_sans_fixed.clone();

        let cards = AppearanceMode::ALL.into_iter().map(|mode| {
            widgets::option_card(&theme, mode.label(), mode == current, preview(mode))
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
        });

        let font_rows: Vec<AnyElement> = availability
            .choices()
            .iter()
            .cloned()
            .enumerate()
            .map(|(ix, family)| {
                let available = availability.is_available(&family);
                let selected = family == effective_font;
                let focused = family == self.selected_font;
                let label = SharedString::from(family.label().to_owned());
                popover::menu_row_nav(
                    &theme,
                    selected,
                    focused,
                    format!("interface-font-option-{ix}"),
                )
                .id(("interface-font-option", ix))
                .when(available, |row| {
                    row.on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.selected_font = family.clone();
                        this.commit_font(cx);
                    }))
                })
                .when(!available, |row| row.opacity(0.45))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(div().w(px(18.0)).flex_none().when(selected, |slot| {
                    slot.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.accent))
                }))
                .into_any_element()
            })
            .collect();

        let font_menu = popover::popover_card(&theme)
            .id("interface-font-scroll")
            .w(px(220.0))
            .font_family(fixed.clone())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_font_menu(cx)))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(font_rows)
            .into_any_element();

        let font_trigger = div()
            .id("interface-font-dropdown")
            .relative()
            .w(px(220.0))
            .h(px(36.0))
            .px(px(11.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if self.font_menu.is_open() {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(crate::theme::ink(0.025))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .track_focus(&self.font_focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_font_key_down(event, cx)),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.font_focus, cx);
                this.toggle_font_menu(cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(effective_font.label().to_owned())),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.font_menu.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "interface-font-menu",
                    font_menu,
                    self.font_menu.closing_since(),
                ))
            });

        let size_rows: Vec<AnyElement> =
            UiFontSize::ALL
                .into_iter()
                .enumerate()
                .map(|(ix, size)| {
                    popover::menu_row_nav(
                        &theme,
                        size == typography::font_size(cx),
                        size == self.selected_size,
                        format!("interface-font-size-option-{ix}"),
                    )
                    .id(("interface-font-size-option", ix))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.selected_size = size;
                        this.commit_size(window, cx);
                    }))
                    .child(div().flex_1().child(size.label()))
                    .child(div().w(px(18.0)).flex_none().when(
                        size == typography::font_size(cx),
                        |slot| {
                            slot.child(icon(icons::CHECK).size(px(14.0)).text_color(theme.accent))
                        },
                    ))
                    .into_any_element()
                })
                .collect();

        let size_menu = popover::popover_card(&theme)
            .w(px(128.0))
            .font_family(fixed.clone())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_size_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(size_rows)
            .into_any_element();

        let size_trigger = div()
            .id("interface-font-size-dropdown")
            .relative()
            .w(px(128.0))
            .h(px(36.0))
            .px(px(11.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if self.size_menu.is_open() {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(crate::theme::ink(0.025))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .track_focus(&self.size_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_size_key_down(event, window, cx)
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.size_focus, cx);
                this.toggle_size_menu(cx);
            }))
            .child(div().flex_1().child(typography::font_size(cx).label()))
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .when_some(self.size_menu.get(), |trigger, _| {
                trigger.child(popover::anchored_menu_below(
                    "interface-font-size-menu",
                    size_menu,
                    self.size_menu.closing_since(),
                ))
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
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .line_height(px(18.0))
                            .child(helper(current, system)),
                    )
                    .child(
                        div()
                            .mt(px(36.0))
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .font_family(fixed.clone())
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(24.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.0))
                                            .child(widgets::field_label(&theme, "Interface font"))
                                            .child(
                                                div()
                                                    .max_w(px(520.0))
                                                    .text_size(typography::ui_rems(12.0))
                                                    .line_height(px(18.0))
                                                    .text_color(theme.text_muted)
                                                    .child(SharedString::from(
                                                        "Used across the interface and conversations. Code, diffs, and terminal keep their current fonts and sizes.",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(font_trigger)
                                            .child(size_trigger),
                                    ),
                            )
                            .when(requested_font != effective_font, |section| {
                                section.child(
                                    widgets::error_strip(
                                        &theme,
                                        "This font could not be loaded. Comet is using Geist.",
                                    )
                                    .font_family(fixed.clone()),
                                )
                            }),
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

    #[test]
    fn font_options_appear_once_in_stable_order() {
        let catalog = FontAvailability::all();
        let labels: Vec<_> = catalog.choices().iter().map(UiFontFamily::label).collect();
        assert_eq!(labels.len(), 5);
        assert_eq!(
            labels,
            ["Geist", "Geist Mono", "System UI", "Arial", "Menlo"]
        );
        let unique = labels.into_iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn font_keyboard_navigation_stops_at_edges_and_skips_unavailable() {
        let all = FontAvailability::all();
        assert_eq!(
            step_font(&UiFontFamily::Geist, -1, &all),
            UiFontFamily::Geist
        );
        assert_eq!(
            step_font(&UiFontFamily::Installed("Menlo".into()), 1, &all),
            UiFontFamily::Installed("Menlo".into())
        );
        let without_arial = all.without(&UiFontFamily::Installed("Arial".into()));
        assert_eq!(
            step_font(&UiFontFamily::System, 1, &without_arial),
            UiFontFamily::Installed("Menlo".into())
        );
    }

    #[test]
    fn font_size_options_are_ordered_and_include_the_default() {
        let values = UiFontSize::ALL.map(UiFontSize::pixels);
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(UiFontSize::ALL.contains(&UiFontSize::default()));
    }
}
