//! Settings → Notifications: independently configurable session chimes plus
//! desktop banners on the same status transitions (`shell::on_state_changed`).
//!
//! The ShortcutsPage arrangement: the page holds a working copy, every flip
//! emits [`NotificationsEvent::Changed`], and the shell persists it. Nothing
//! here talks RPC — all preferences are device-local UI settings.

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use crate::icons;
use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum NotificationsEvent {
    /// A toggle flipped — persist the complete notification preference set.
    Changed {
        sound: bool,
        completion_sound: bool,
        input_sound: bool,
        attention_sound: bool,
        desktop: bool,
        background_only: bool,
    },
}

pub struct NotificationsPage {
    sound: bool,
    completion_sound: bool,
    input_sound: bool,
    attention_sound: bool,
    desktop: bool,
    background_only: bool,
}

impl EventEmitter<NotificationsEvent> for NotificationsPage {}

#[derive(Clone, Copy)]
enum NotificationPreference {
    Sound,
    CompletionSound,
    InputSound,
    AttentionSound,
    Desktop,
    BackgroundOnly,
}

fn is_switch_activation(key: &str, is_held: bool) -> bool {
    !is_held && matches!(key, "enter" | "space")
}

fn interactive_switch(
    element: gpui::Stateful<gpui::Div>,
    accent: gpui::Hsla,
    preference: NotificationPreference,
    cx: &mut Context<NotificationsPage>,
) -> gpui::Stateful<gpui::Div> {
    element
        .tab_index(0)
        .focus_visible(move |style| style.border_2().border_color(accent))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle(preference, cx);
        }))
        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
            if is_switch_activation(&event.keystroke.key, event.is_held) {
                cx.stop_propagation();
                this.toggle(preference, cx);
            }
        }))
}

impl NotificationsPage {
    pub fn new(
        sound: bool,
        completion_sound: bool,
        input_sound: bool,
        attention_sound: bool,
        desktop: bool,
        background_only: bool,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            sound,
            completion_sound,
            input_sound,
            attention_sound,
            desktop,
            background_only,
        }
    }

    fn emit(&self, cx: &mut Context<Self>) {
        cx.emit(NotificationsEvent::Changed {
            sound: self.sound,
            completion_sound: self.completion_sound,
            input_sound: self.input_sound,
            attention_sound: self.attention_sound,
            desktop: self.desktop,
            background_only: self.background_only,
        });
    }

    fn toggle(&mut self, preference: NotificationPreference, cx: &mut Context<Self>) {
        let value = match preference {
            NotificationPreference::Sound => &mut self.sound,
            NotificationPreference::CompletionSound => &mut self.completion_sound,
            NotificationPreference::InputSound => &mut self.input_sound,
            NotificationPreference::AttentionSound => &mut self.attention_sound,
            NotificationPreference::Desktop => &mut self.desktop,
            NotificationPreference::BackgroundOnly => &mut self.background_only,
        };
        *value = !*value;
        self.emit(cx);
        cx.notify();
    }
}

impl Render for NotificationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let accent = theme.accent;
        let sound = self.sound;
        let completion_sound = self.completion_sound;
        let input_sound = self.input_sound;
        let attention_sound = self.attention_sound;
        let desktop = self.desktop;
        let background_only = self.background_only;
        let toggle = |id: &'static str, label: &'static str, enabled: bool, interactive: bool| {
            // Keep the familiar 32×18 visual inside a 40×40 activation target.
            // Disabled subordinate controls remain named switches in the
            // accessibility tree, but have no focus or input handlers.
            div()
                .id(id)
                .flex_none()
                .size(px(40.0))
                .flex()
                .items_center()
                .justify_center()
                .role(gpui::Role::Switch)
                .aria_label(label)
                .aria_toggled(if enabled {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .when(!interactive, |el| {
                    el.aria_description("Unavailable while its parent setting is off")
                })
                .child(widgets::toggle_switch(&theme, enabled))
        };
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
                            .child(widgets::row_title(&theme, "Session sounds"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Allow sounds for the selected session events below.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        interactive_switch(
                            toggle("notifications-sound-toggle", "Session sounds", sound, true),
                            accent,
                            NotificationPreference::Sound,
                            cx,
                        ),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .when(!sound, |el| el.opacity(0.55))
                    .child(widgets::row_tile(&theme, icons::CHECK))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Task completed"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Play a sound when an agent finishes a run.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        toggle(
                            "notifications-completion-sound-toggle",
                            "Task completed sound",
                            completion_sound,
                            sound,
                        )
                        .when(sound, |el| {
                            interactive_switch(
                                el,
                                accent,
                                NotificationPreference::CompletionSound,
                                cx,
                            )
                        }),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .when(!sound, |el| el.opacity(0.55))
                    .child(widgets::row_tile(&theme, icons::CHAT_ROUND_LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Input required"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Play a sound when an agent needs your response.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        toggle(
                            "notifications-input-sound-toggle",
                            "Input required sound",
                            input_sound,
                            sound,
                        )
                        .when(sound, |el| {
                            interactive_switch(
                                el,
                                accent,
                                NotificationPreference::InputSound,
                                cx,
                            )
                        }),
                    ),
            )
            .child(
                widgets::card_row(&theme, false)
                    .when(!sound, |el| el.opacity(0.55))
                    .child(widgets::row_tile(&theme, icons::DANGER_TRIANGLE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, "Errors and disconnections"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div()
                                    .child(SharedString::from(
                                        "Play a sound when a run fails or the connection remains unavailable.",
                                    ))
                                    .into_any_element()],
                            )),
                    )
                    .child(
                        toggle(
                            "notifications-attention-sound-toggle",
                            "Errors and disconnections sound",
                            attention_sound,
                            sound,
                        )
                        .when(sound, |el| {
                            interactive_switch(
                                el,
                                accent,
                                NotificationPreference::AttentionSound,
                                cx,
                            )
                        }),
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
                            .child(widgets::row_title(&theme, "Desktop notifications"))
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
                        interactive_switch(
                            toggle(
                                "notifications-desktop-toggle",
                                "Desktop notifications",
                                desktop,
                                true,
                            ),
                            accent,
                            NotificationPreference::Desktop,
                            cx,
                        ),
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
                                            "Skip the banner while a Zeron window is focused.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        toggle(
                            "notifications-background-toggle",
                            "Only notify when Zeron is in the background",
                            background_only,
                            desktop,
                        )
                        .when(desktop, |el| {
                            interactive_switch(
                                el,
                                accent,
                                NotificationPreference::BackgroundOnly,
                                cx,
                            )
                        }),
                    ),
            );

        div()
            .id("notifications-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Notifications", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose which session events can play a sound, and when desktop \
                             notifications appear.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(card),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::is_switch_activation;

    #[test]
    fn switches_accept_enter_or_space_once_per_press() {
        assert!(is_switch_activation("enter", false));
        assert!(is_switch_activation("space", false));
        assert!(!is_switch_activation("escape", false));
        assert!(!is_switch_activation("space", true));
    }
}
