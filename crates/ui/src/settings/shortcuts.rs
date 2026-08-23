//! Settings → Shortcuts (feature-inventory §1.4): a table of the rebindable
//! bindings — click a combo to record (Esc cancels), live conflict detection,
//! per-row Reset and Restore defaults. Changes emit [`ShortcutsEvent::Changed`];
//! the shell persists them and re-applies the app keymap.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::appshots::{AppshotCapabilities, AppshotDestination, CapabilityState};
use crate::settings::{KeymapConfig, ShortcutId, combo_from_keystroke, display_combo};
use crate::state::AppState;
use crate::theme::Theme;

/// Outcome of one keystroke while recording. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Esc — abandon recording, keep the old combo.
    Cancelled,
    /// A bare modifier (or unusable key) — stay recording.
    Ignored,
    /// A full combo landed.
    Set(String),
}

pub fn record_key(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> RecordOutcome {
    if key.eq_ignore_ascii_case("escape") {
        return RecordOutcome::Cancelled;
    }
    match combo_from_keystroke(ctrl, alt, shift, cmd, key) {
        Some(combo) => RecordOutcome::Set(combo),
        None => RecordOutcome::Ignored,
    }
}

#[derive(Debug, Clone)]
pub enum ShortcutsEvent {
    /// The keymap changed — persist + re-apply.
    Changed(KeymapConfig),
    AppshotsChanged {
        enabled: bool,
        destination: AppshotDestination,
    },
}

pub struct ShortcutsPage {
    /// Working copy (kept in sync with the shell via `Changed` events).
    keymap: KeymapConfig,
    recording: Option<ShortcutId>,
    /// A rejected record attempt ("{Combo} is already assigned to {label}.") —
    /// conflicts never persist; they're refused at record time, as in zeron.
    conflict_notice: Option<SharedString>,
    focus: FocusHandle,
    appshots_enabled: bool,
    appshot_destination: AppshotDestination,
    appshot_capabilities: AppshotCapabilities,
    capture_access_prompted: bool,
    semantic_access_prompted: bool,
    // The page never talks RPC; state is kept for parity with sibling pages
    // (and future per-device keymaps).
    _state: Entity<AppState>,
}

impl EventEmitter<ShortcutsEvent> for ShortcutsPage {}

impl ShortcutsPage {
    pub fn new(
        state: Entity<AppState>,
        keymap: KeymapConfig,
        appshots_enabled: bool,
        appshot_destination: AppshotDestination,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            keymap,
            recording: None,
            conflict_notice: None,
            focus: cx.focus_handle(),
            appshots_enabled,
            appshot_destination,
            appshot_capabilities: crate::appshots::capabilities(),
            capture_access_prompted: false,
            semantic_access_prompted: false,
            _state: state,
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::Changed(self.keymap.clone()));
        cx.notify();
    }

    fn commit_appshots(&self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::AppshotsChanged {
            enabled: self.appshots_enabled,
            destination: self.appshot_destination,
        });
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(recording) = self.recording else {
            return;
        };
        let mods = &event.keystroke.modifiers;
        match record_key(
            &event.keystroke.key,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        ) {
            RecordOutcome::Cancelled => {
                self.recording = None;
                cx.notify();
            }
            RecordOutcome::Ignored => {}
            RecordOutcome::Set(combo) => {
                // A combo already bound elsewhere is REFUSED, naming the owner
                // (zeron settings.shortcuts.tsx: "… is already assigned to …").
                if let Some(owner) = conflict_owner(&self.keymap, recording, &combo) {
                    self.conflict_notice = Some(
                        format!(
                            "{} is already assigned to {}.",
                            display_combo(&combo),
                            owner.label()
                        )
                        .into(),
                    );
                    self.recording = None;
                    cx.notify();
                } else {
                    self.keymap.set(recording, combo);
                    self.recording = None;
                    self.conflict_notice = None;
                    self.commit(cx);
                }
            }
        }
        cx.stop_propagation();
    }
}

/// The shortcut (other than `id`) already bound to `combo`, if any. Pure.
pub fn conflict_owner(keymap: &KeymapConfig, id: ShortcutId, combo: &str) -> Option<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .find(|&other| other != id && keymap.get(other) == combo)
}

/// One-line purpose copy per shortcut (zeron lib/shortcuts.ts
/// `SHORTCUT_DEFINITIONS` descriptions, verbatim).
fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::ToggleSidebar => "Show or hide sessions and settings navigation.",
        ShortcutId::ToggleChanges => "Show or hide changes for the current session.",
        ShortcutId::ToggleTerminal => "Show or hide the terminal for the current session.",
        ShortcutId::NewSession => "Open a blank session canvas to start a new session.",
        ShortcutId::NextSession => "Select the next session in the sidebar, wrapping at the end.",
        ShortcutId::PrevSession => {
            "Select the previous session in the sidebar, wrapping at the start."
        }
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        self.appshot_capabilities = crate::appshots::capabilities();
        let theme = Theme::of(cx).clone();
        let recording = self.recording;
        let customized = self.keymap != KeymapConfig::default();
        let appshots_enabled = self.appshots_enabled;
        let capabilities = self.appshot_capabilities;

        let rows = ShortcutId::ALL.into_iter().enumerate().map(|(ix, id)| {
            let combo = self.keymap.get(id).to_string();
            let is_recording = recording == Some(id);
            let non_default = combo != id.default_combo();
            let chip_text: SharedString = if is_recording {
                "Press keys…".into()
            } else {
                display_combo(&combo).into()
            };
            // zeron settings.shortcuts.tsx row: min-h-[72px] px-5 gap-5, label
            // + description left, Reset (only when modified), then the combo
            // chip — recording inverts it to white-on-black.
            div()
                .min_h(px(72.0))
                .px(px(20.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(20.0))
                .when(ix > 0, |el| el.border_t_1().border_color(theme.border))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(id.label())),
                        )
                        .child(
                            div()
                                .mt(px(2.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(description(id))),
                        ),
                )
                .when(non_default && !is_recording, |el| {
                    el.child(
                        div()
                            .id(("shortcut-reset", ix))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.keymap.reset(id);
                                this.recording = None;
                                this.commit(cx);
                            }))
                            .child(SharedString::from("Reset")),
                    )
                })
                .child(
                    div()
                        .id(("shortcut-combo", ix))
                        .min_w(px(96.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .flex()
                        .justify_center()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .map(|el| {
                            if is_recording {
                                el.border_color(theme.text.opacity(0.3))
                                    .bg(theme.text)
                                    .text_color(theme.on_solid)
                            } else {
                                el.border_color(theme.border)
                                    .bg(theme.bg)
                                    .text_color(theme.text)
                                    .hover(|s| {
                                        // `hover:border-foreground/20` — the
                                        // neutral foreground, not pure white.
                                        s.border_color(theme.text.opacity(0.2))
                                            .bg(crate::theme::ink(0.03))
                                    })
                            }
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.recording = Some(id);
                            this.conflict_notice = None;
                            window.focus(&this.focus, cx);
                            cx.notify();
                        }))
                        .child(chip_text),
                )
        });

        // Helper line stays in the muted tone even for a rejected conflict —
        // the message names the specific clash (zeron settings.shortcuts.tsx).
        let helper: SharedString = if recording.is_some() {
            "Press Escape to cancel.".into()
        } else if let Some(notice) = self.conflict_notice.clone() {
            notice
        } else {
            "Shortcuts must be unique.".into()
        };

        let appshot_destination =
            AppshotDestination::ALL
                .into_iter()
                .enumerate()
                .map(|(ix, destination)| {
                    let selected = self.appshot_destination == destination;
                    div()
                        .id(("appshot-destination", ix))
                        .px(px(10.0))
                        .py(px(6.0))
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
                        .child(SharedString::from(destination.label()))
                });
        let capture_action: SharedString = if self.capture_access_prompted {
            "Open System Settings".into()
        } else {
            "Allow".into()
        };
        let semantic_action: SharedString = if self.semantic_access_prompted {
            "Open System Settings".into()
        } else {
            "Enable text capture".into()
        };

        div()
            .id("shortcuts-page")
            .size_full()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key_down(event, cx)),
            )
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(widgets::page_header(&theme, "Keyboard shortcuts", None))
                                    .child(
                                        widgets::page_subtitle(
                                            &theme,
                                            "Click a binding, then press the key combination you \
                                             want to use. Changes apply immediately and stay on \
                                             this device.",
                                        )
                                        .max_w(px(512.0))
                                        .line_height(px(20.0)),
                                    ),
                            )
                            .child({
                                // `disabled:opacity-35` when nothing is
                                // customized or while recording.
                                let disabled = !customized || recording.is_some();
                                widgets::ghost_action(&theme)
                                    .id("shortcuts-restore-defaults")
                                    .flex_none()
                                    .when(disabled, |el| el.opacity(0.35))
                                    .when(!disabled, |el| {
                                        el.hover(|s| {
                                            s.bg(crate::theme::ink(0.04)).text_color(theme.text)
                                        })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.keymap = KeymapConfig::default();
                                                this.recording = None;
                                                this.conflict_notice = None;
                                                this.commit(cx);
                                            }),
                                        )
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::RESTART)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Restore defaults"))
                            }),
                    )
                    .child(widgets::section_card(&theme).mt(px(32.0)).children(rows))
                    .child(
                        div()
                            .mt(px(12.0))
                            .px(px(4.0))
                            .min_h(px(20.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(helper),
                    )
                    .child(
                        div()
                            .mt(px(36.0))
                            .child(widgets::page_header(&theme, "Appshots", None))
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    capabilities.setup_description(),
                                )
                                .max_w(px(560.0))
                                .line_height(px(20.0)),
                            ),
                    )
                    .child(
                        widgets::section_card(&theme)
                            .child(
                                widgets::card_row(&theme, true)
                                    .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                    .child(widgets::row_title(&theme, "Capture Appshots"))
                                    .child(widgets::meta_line(
                                        &theme,
                                        vec![div().child(SharedString::from("Captures are staged for review and never sent automatically.")).into_any_element()],
                                    )),
                                    )
                                    .child(
                                        widgets::toggle_switch(&theme, appshots_enabled)
                                            .id("appshots-enabled")
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.appshots_enabled = !this.appshots_enabled;
                                                this.commit_appshots(cx);
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                widgets::card_row(&theme, false)
                                    .when(!appshots_enabled, |el| el.opacity(0.55))
                                    .child(widgets::row_tile(&theme, crate::icons::KEYBOARD))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap(px(8.0))
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Global shortcut",
                                                    ))
                                                    .child(if capabilities.global_shortcut
                                                        == CapabilityState::Ready
                                                    {
                                                        widgets::badge_active(
                                                            &theme,
                                                            capabilities.global_shortcut.badge(),
                                                        )
                                                    } else {
                                                        widgets::badge(
                                                            &theme,
                                                            capabilities.global_shortcut.badge(),
                                                        )
                                                    }),
                                            )
                                            .child(widgets::meta_line(
                                                &theme,
                                                vec![div()
                                                    .child(SharedString::from(format!(
                                                        "{} · {}",
                                                        capabilities.shortcut_label(),
                                                        capabilities.shortcut_description()
                                                    )))
                                                    .into_any_element()],
                                            )),
                                    ),
                            )
                            .child(
                                widgets::card_row(&theme, false)
                                    .when(!appshots_enabled, |el| el.opacity(0.55))
                                    .child(widgets::row_tile(&theme, crate::icons::KEYBOARD))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(widgets::row_title(&theme, "Destination"))
                                            .child(widgets::meta_line(
                                                &theme,
                                                vec![div().child(SharedString::from("Choose where a completed capture is staged.")).into_any_element()],
                                            )),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .gap(px(6.0))
                                            .children(appshot_destination),
                                    ),
                            )
                            .child(
                                widgets::card_row(&theme, false)
                                    .when(!appshots_enabled, |el| el.opacity(0.55))
                                    .child(widgets::row_tile(&theme, crate::icons::MONITOR))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap(px(8.0))
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Active-window capture",
                                                    ))
                                                    .child(if capabilities.window_capture
                                                        == CapabilityState::Ready
                                                    {
                                                        widgets::badge_active(
                                                            &theme,
                                                            capabilities.window_capture.badge(),
                                                        )
                                                    } else {
                                                        widgets::badge(
                                                            &theme,
                                                            capabilities.window_capture.badge(),
                                                        )
                                                    }),
                                            )
                                            .child(widgets::meta_line(
                                                &theme,
                                                vec![div().child(SharedString::from(capabilities.capture_description())).into_any_element()],
                                            )),
                                    )
                                    .when(
                                        appshots_enabled
                                            && capabilities.window_capture
                                                == CapabilityState::PermissionRequired
                                            && crate::appshots::capture_settings_url().is_some(),
                                        |el| {
                                        el.child(
                                            widgets::ghost_action(&theme)
                                                .id("appshots-capture-access")
                                                .cursor_pointer()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    if this.capture_access_prompted {
                                                        if let Some(url) = crate::appshots::capture_settings_url() {
                                                            cx.open_url(url);
                                                        }
                                                    } else {
                                                        this.capture_access_prompted = true;
                                                        crate::appshots::request_capture_access();
                                                        this.appshot_capabilities = crate::appshots::capabilities();
                                                    }
                                                    cx.notify();
                                                }))
                                                .child(capture_action),
                                        )
                                    },
                                    )
                            )
                            .child(
                                widgets::card_row(&theme, false)
                                    .when(!appshots_enabled, |el| el.opacity(0.55))
                                    .child(widgets::row_tile(&theme, crate::icons::CHECKLIST))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_row()
                                                    .items_center()
                                                    .gap(px(8.0))
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Application text",
                                                    ))
                                                    .child(if capabilities.application_text
                                                        == CapabilityState::Ready
                                                    {
                                                        widgets::badge_active(
                                                            &theme,
                                                            capabilities.application_text.badge(),
                                                        )
                                                    } else {
                                                        widgets::badge(
                                                            &theme,
                                                            capabilities.application_text.badge(),
                                                        )
                                                    }),
                                            )
                                            .child(widgets::meta_line(
                                                &theme,
                                                vec![div().child(SharedString::from(capabilities.semantic_description())).into_any_element()],
                                            )),
                                    )
                                    .when(
                                        appshots_enabled
                                            && capabilities.application_text
                                                == CapabilityState::PermissionRequired
                                            && crate::appshots::semantic_settings_url().is_some(),
                                        |el| {
                                        el.child(
                                            widgets::ghost_action(&theme)
                                                .id("appshots-semantic-access")
                                                .cursor_pointer()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    if this.semantic_access_prompted {
                                                        if let Some(url) = crate::appshots::semantic_settings_url() {
                                                            cx.open_url(url);
                                                        }
                                                    } else {
                                                        this.semantic_access_prompted = true;
                                                        crate::appshots::request_semantic_access();
                                                        this.appshot_capabilities = crate::appshots::capabilities();
                                                    }
                                                    cx.notify();
                                                }))
                                                .child(semantic_action),
                                        )
                                    },
                                    )
                            )
                            .when(
                                appshots_enabled
                                    && !(capabilities.global_shortcut.is_ready()
                                        && capabilities.window_capture.is_ready()
                                        && capabilities.application_text.is_ready()),
                                |card| {
                                    card.child(
                                        widgets::card_row(&theme, false)
                                            .child(div().w(px(36.0)).flex_none())
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_size(px(11.5))
                                                    .text_color(theme.text_muted.opacity(0.65))
                                                    .child(SharedString::from("Changed a permission or desktop shortcut setting? Return here and check again.")),
                                            )
                                            .child(
                                                widgets::ghost_action(&theme)
                                                    .id("appshots-refresh-permissions")
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.appshot_capabilities = crate::appshots::capabilities();
                                                        cx.notify();
                                                    }))
                                                    .child(SharedString::from("Check again")),
                                            ),
                                    )
                                },
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_outcomes() {
        assert_eq!(
            record_key("escape", false, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("Escape", true, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("s", false, false, false, true),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        // macOS-only: elsewhere ctrl IS the primary and records as "mod".
        #[cfg(target_os = "macos")]
        assert_eq!(
            record_key("tab", true, false, true, false),
            RecordOutcome::Set("ctrl-shift-tab".into())
        );
        // Bare modifiers stay recording.
        assert_eq!(
            record_key("shift", false, false, true, false),
            RecordOutcome::Ignored
        );
        assert_eq!(
            record_key("ctrl", true, false, false, false),
            RecordOutcome::Ignored
        );
    }

    #[test]
    fn conflicting_records_are_refused() {
        // zeron parity: a combo bound elsewhere is refused at record time (the
        // helper names the owner) — conflicts never persist into the keymap.
        let keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("b", false, false, false, true) else {
            panic!("expected Set");
        };
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, &combo),
            Some(ShortcutId::ToggleChanges)
        );
        // Re-recording a shortcut's own combo is not a conflict.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleChanges, &combo),
            None
        );
        // A free combo conflicts with nothing.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-shift-x"),
            None
        );
    }
}
