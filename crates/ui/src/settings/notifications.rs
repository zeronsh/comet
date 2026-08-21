//! Settings → Notifications: the session ping toggles — the completion/
//! question chime and the desktop banner ride the same status transitions
//! (`shell::on_state_changed`); this page flips their two `UiSettings` flags.
//!
//! The ShortcutsPage arrangement: the page holds a working copy, every flip
//! emits [`NotificationsEvent::Changed`], and the shell persists it. Nothing
//! here talks RPC — both flags are device-local UI settings.

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use crate::i18n::t;
use crate::icons;
use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum NotificationsEvent {
    /// A toggle flipped — persist all three flags.
    Changed {
        sound: bool,
        desktop: bool,
        background_only: bool,
    },
}

pub struct NotificationsPage {
    sound: bool,
    desktop: bool,
    background_only: bool,
}

impl EventEmitter<NotificationsEvent> for NotificationsPage {}

impl NotificationsPage {
    pub fn new(sound: bool, desktop: bool, background_only: bool, _cx: &mut Context<Self>) -> Self {
        Self {
            sound,
            desktop,
            background_only,
        }
    }

    fn emit(&self, cx: &mut Context<Self>) {
        cx.emit(NotificationsEvent::Changed {
            sound: self.sound,
            desktop: self.desktop,
            background_only: self.background_only,
        });
    }
}

impl Render for NotificationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let sound = self.sound;
        let desktop = self.desktop;
        let background_only = self.background_only;
        let card = widgets::section_card(&theme)
            .child(
                widgets::card_row(&theme, true)
                    .child(widgets::row_tile(&theme, icons::VOLUME_LOUD))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, t("Sounds")))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            t("Chime when a run finishes or an agent asks a question."),
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, sound)
                            .id("notifications-sound-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.sound = !this.sound;
                                this.emit(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .child(widgets::row_tile(&theme, icons::BELL))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, t("Desktop notifications")))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Show a system banner on the same events, so pings \
                                             reach you while Zeron is in the background.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, desktop)
                            .id("notifications-desktop-toggle")
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.desktop = !this.desktop;
                                this.emit(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                // Sub-option of the banner row: dimmed + inert while banners
                // are off (the harnesses not-installed treatment).
                widgets::card_row(&theme, false)
                    .when(!desktop, |el| el.opacity(0.55))
                    .child(widgets::row_tile(&theme, icons::MONITOR))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Only when in the background"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Skip the banner while a Zeron window is focused — \
                                             the chime already covers it.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(&theme, background_only)
                            .id("notifications-background-toggle")
                            .when(desktop, |el| {
                                el.cursor_pointer().on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.background_only = !this.background_only;
                                        this.emit(cx);
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
            );

        div()
            .id("notifications-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, t("Notifications"), None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "How session pings reach you when a run finishes or an agent is \
                             waiting on your input.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(card),
            )
    }
}
