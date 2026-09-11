//! Appshots settings share the shortcut recorder and persistence events with
//! keyboard settings, while keeping capture setup on its own route.
use super::*;
use crate::appshots::{AppshotPlatform, CapabilityState};
use crate::settings::widgets;

fn row(
    theme: &Theme,
    first: bool,
    title: &str,
    description: &str,
    control: gpui::AnyElement,
) -> gpui::Div {
    widgets::card_row(theme, first)
        .flex_wrap()
        .child(
            div()
                .flex_1()
                .min_w(px(160.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(widgets::row_title(theme, title.to_string()))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .line_height(px(18.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(description.to_string())),
                ),
        )
        .child(control)
}

impl ShortcutsPage {
    pub(super) fn render_appshots(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let accent = theme.accent;
        let capabilities = self.appshot_capabilities;
        let toggle = |id: &'static str, label: &'static str, enabled: bool| {
            widgets::toggle_switch(&theme, enabled)
                .id(id)
                .role(gpui::Role::Switch)
                .aria_label(label)
                .aria_toggled(if enabled {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .tab_index(0)
                .focus_visible(move |style| style.border_2().border_color(accent))
        };
        let capture = toggle(
            "appshots-enabled",
            "Capture Appshots",
            self.appshots_enabled,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.appshots_enabled = !this.appshots_enabled;
            this.commit_appshots(cx);
            cx.notify();
        }));
        let sound = toggle(
            "appshot-sound-enabled",
            "Capture sound",
            self.appshot_sound_enabled,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.appshot_sound_enabled = !this.appshot_sound_enabled;
            this.commit_appshots(cx);
            cx.notify();
        }));
        let destinations = AppshotDestination::ALL
            .into_iter()
            .enumerate()
            .map(|(ix, destination)| {
                let selected = self.appshot_destination == destination;
                div()
                    .id(("appshot-destination", ix))
                    .role(gpui::Role::Button)
                    .aria_label(destination.label())
                    .aria_toggled(if selected {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .tab_index(0)
                    .focus_visible(move |style| style.border_2().border_color(accent))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(if selected {
                        theme.text.opacity(0.24)
                    } else {
                        theme.border
                    })
                    .bg(if selected {
                        crate::theme::ink(0.09)
                    } else {
                        gpui::transparent_black()
                    })
                    .text_size(px(11.0))
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.appshot_destination = destination;
                        this.commit_appshots(cx);
                        cx.notify();
                    }))
                    .child(destination.label())
            })
            .collect::<Vec<_>>();
        let destination_description = match self.appshot_destination {
            AppshotDestination::Automatic => {
                "Use the open session, or the new-session composer when no session is open."
            }
            AppshotDestination::LastSession => {
                "Use the open session, or return to the last session used for an Appshot."
            }
            AppshotDestination::NewSession => {
                "Stage captures in a new-session composer, keeping existing drafts intact."
            }
        };
        let shortcut = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(widgets::badge(&theme, capabilities.global_shortcut.badge()))
            .child(self.render_binding_control(
                ShortcutId::CaptureAppshot,
                0,
                self.recording,
                &theme,
                cx,
            ));
        let action = |id: &'static str, label: &'static str| {
            widgets::ghost_action(&theme)
                .id(id)
                .role(gpui::Role::Button)
                .aria_label(label)
                .tab_index(0)
                .focus_visible(move |style| style.border_2().border_color(accent))
                .cursor_pointer()
                .child(label)
        };
        let mut window_access = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(widgets::badge(&theme, capabilities.window_capture.badge()));
        if self.appshots_enabled
            && capabilities.window_capture == CapabilityState::PermissionRequired
        {
            let label = if self.capture_access_prompted {
                "Open System Settings"
            } else {
                "Allow window capture"
            };
            window_access = window_access.child(action("appshots-capture-access", label).on_click(
                cx.listener(|this, _, _, cx| {
                    if this.capture_access_prompted {
                        if let Some(url) = crate::appshots::capture_settings_url() {
                            cx.open_url(url);
                        }
                    } else {
                        this.capture_access_prompted = true;
                        crate::appshots::request_capture_access();
                    }
                    this.appshot_capabilities = crate::appshots::capabilities();
                    cx.notify();
                }),
            ));
        }
        let mut text_access = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(widgets::badge(
                &theme,
                capabilities.application_text.badge(),
            ));
        if self.appshots_enabled
            && capabilities.application_text == CapabilityState::PermissionRequired
            && crate::appshots::semantic_settings_url().is_some()
        {
            let label = if self.semantic_access_prompted {
                "Open System Settings"
            } else {
                "Enable text capture"
            };
            text_access = text_access.child(action("appshots-semantic-access", label).on_click(
                cx.listener(|this, _, _, cx| {
                    if this.semantic_access_prompted {
                        if let Some(url) = crate::appshots::semantic_settings_url() {
                            cx.open_url(url);
                        }
                    } else {
                        this.semantic_access_prompted = true;
                        crate::appshots::request_semantic_access();
                    }
                    this.appshot_capabilities = crate::appshots::capabilities();
                    cx.notify();
                }),
            ));
        }
        let card = widgets::section_card(&theme)
            .child(row(
                &theme,
                true,
                "Capture Appshots",
                "Captures are staged for review and never sent automatically.",
                capture.into_any_element(),
            ))
            .child(row(
                &theme,
                false,
                "Capture sound",
                "Play a sound when an Appshot is ready.",
                sound.into_any_element(),
            ))
            .child(row(
                &theme,
                false,
                if capabilities.platform == AppshotPlatform::LinuxWayland {
                    "Preferred shortcut"
                } else {
                    "Global shortcut"
                },
                capabilities.shortcut_description(),
                shortcut.into_any_element(),
            ))
            .child(row(
                &theme,
                false,
                "Destination",
                destination_description,
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(destinations)
                    .into_any_element(),
            ))
            .child(row(
                &theme,
                false,
                "Window capture",
                capabilities.capture_description(),
                window_access.into_any_element(),
            ))
            .child(row(
                &theme,
                false,
                "Application text",
                capabilities.semantic_description(),
                text_access.into_any_element(),
            ));
        let helper: SharedString = if self.recording.is_some() {
            "Press Escape to cancel.".into()
        } else {
            self.conflict_notice.clone().unwrap_or_default()
        };
        let refresh = action("appshots-refresh-permissions", "Check again")
            .aria_label("Check permissions again")
            .on_click(cx.listener(|this, _, _, cx| {
                this.appshot_capabilities = crate::appshots::capabilities();
                cx.notify();
            }));
        div().id("appshots-settings-page").size_full().overflow_y_scroll().track_focus(&self.focus)
            .tab_group()
            .tab_index(0)
            .tab_stop(false)
            .on_key_down(|event, window, cx| {
                let key = &event.keystroke;
                if key.key == "tab" && !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
                    if key.modifiers.shift { window.focus_prev(cx); } else { window.focus_next(cx); }
                    cx.stop_propagation();
                }
            })
            .child(widgets::page_column().child(widgets::page_header(&theme, "Appshots", None))
                .child(widgets::page_subtitle(&theme, capabilities.setup_description()).line_height(px(20.0)))
                .child(card).child(div().min_h(px(20.0)).mt(px(8.0)).text_size(px(12.0)).text_color(theme.text_muted).child(helper))
                .child(div().mt(px(12.0)).flex().flex_wrap().items_center().gap(px(12.0))
                    .child(div().flex_1().text_size(px(12.0)).text_color(theme.text_muted).child("Changed a permission? Check again after returning to Zeron."))
                    .child(refresh)))
            .into_any_element()
    }
}
