//! Session navigation — the horizontal tab strip is gone (wing 2026-08-10):
//! the activity sidebar IS the session list, and the titlebar names the
//! selected session (harness brand icon + title). When the sidebar is
//! collapsed, a `+` new-session button fades into the titlebar's left end
//! (riding the sidebar width tween). `UiSettings.open_tabs` is legacy — no
//! longer read or written.

use super::*;

fn editor_icon(editor: &str) -> &'static str {
    match editor {
        "cursor" => crate::icons::EDITOR_CURSOR,
        "code" => crate::icons::EDITOR_VSCODE,
        "zed" => crate::icons::EDITOR_ZED,
        "windsurf" => crate::icons::EDITOR_WINDSURF,
        "idea" => crate::icons::EDITOR_INTELLIJ,
        "pycharm" => crate::icons::EDITOR_PYCHARM,
        "goland" => crate::icons::EDITOR_GOLAND,
        "webstorm" => crate::icons::EDITOR_WEBSTORM,
        "rustrover" => crate::icons::EDITOR_RUSTROVER,
        _ => crate::icons::FOLDER,
    }
}

impl Shell {
    /// Titlebar split control for the currently selected thread checkout.
    /// The primary action uses the first installed editor; the arrow exposes
    /// every detected editor plus Finder.
    fn render_editor_control(
        &mut self,
        path: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editors = self.installed_editor_choices();
        let preferred_editor = self.settings.preferred_editor.clone();
        let default_editor = preferred_editor
            .as_deref()
            .filter(|preferred| {
                preferred.is_empty() || editors.iter().any(|(id, _)| id == preferred)
            })
            .unwrap_or_else(|| editors.first().map(|(id, _)| *id).unwrap_or(""))
            .to_string();
        let menu_open = self.editor_menu.get().is_some();
        let menu_closing = self.editor_menu.closing_since();
        let has_editors = !editors.is_empty();
        let default_label = if default_editor.is_empty() {
            "Finder"
        } else {
            editors
                .iter()
                .find(|(id, _)| *id == default_editor.as_str())
                .map(|(_, label)| *label)
                .unwrap_or("Finder")
        };
        let default_icon = editor_icon(&default_editor);

        let mut menu = popover::popover_card(theme)
            .w(px(190.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.close_editor_menu(cx);
            }))
            .flex()
            .flex_col()
            .gap(px(2.0));

        for (id, label) in editors {
            let editor_id = id;
            let editor_path = path.clone();
            let row_id: SharedString = format!("session-editor-{editor_id}").into();
            menu = menu.child(
                popover::menu_row(theme, editor_id == default_editor.as_str(), row_id.clone())
                    .id(row_id)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_preferred_editor(editor_id, cx);
                        open_in_editor(&editor_path, editor_id);
                        this.close_editor_menu(cx);
                    }))
                    .child(
                        icon(editor_icon(editor_id))
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().child(SharedString::from(label))),
            );
        }

        if has_editors {
            menu = menu.child(
                div()
                    .h(px(1.0))
                    .mx(px(8.0))
                    .bg(crate::theme::hairline(0.08)),
            );
        }
        let finder_path = path.clone();
        menu = menu.child(
            popover::menu_row(theme, default_editor.is_empty(), "session-editor-finder")
                .id("session-editor-finder")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_preferred_editor("", cx);
                    open_in_editor(&finder_path, "");
                    this.close_editor_menu(cx);
                }))
                .child(
                    icon(crate::icons::FOLDER)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().child(SharedString::from("Finder"))),
        );

        let primary_path = path.clone();
        let primary_editor = default_editor.clone();
        let mut control = div()
            .id("session-open-editor")
            .relative()
            .flex_none()
            .h(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .rounded(px(7.0))
            .overflow_hidden()
            .bg(theme.glass_hover().opacity(0.55));

        control = control.child(
            div()
                .id("session-open-editor-main")
                .h_full()
                .px(px(9.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .hover(|s| s.bg(theme.glass_hover()))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(
                        move |_this: &mut Shell,
                              _: &gpui::MouseDownEvent,
                              _: &mut Window,
                              _: &mut Context<Shell>| {
                            open_in_editor(&primary_path, &primary_editor);
                        },
                    ),
                )
                .child(
                    icon(default_icon)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(default_label)),
                ),
        );
        control = control.child(
            div()
                .w(px(1.0))
                .h(px(16.0))
                .bg(crate::theme::hairline(0.12)),
        );
        control = control.child(
            div()
                .id("session-open-editor-menu-trigger")
                .h_full()
                .w(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(theme.glass_hover()))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| this.editor_menu.note_trigger_press()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    if this.editor_menu.take_press_was_open() {
                        this.close_editor_menu(cx);
                    } else {
                        this.editor_menu.open(());
                        cx.notify();
                    }
                }))
                .child(
                    icon(crate::icons::ALT_ARROW_DOWN)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                ),
        );

        if menu_open {
            control = control.child(popover::anchored_menu_below_gap(
                "session-editor-menu",
                menu.into_any_element(),
                menu_closing,
                8.0,
            ));
        }

        control.into_any_element()
    }

    /// Boot landing: the most recently active visible chat once the first
    /// chats frame has synced (manual selection wins; no chats → the
    /// new-session canvas shows).
    pub(super) fn boot_select_chat(&mut self, cx: &mut Context<Self>) {
        let first = {
            let state = self.state.read(cx);
            if !state.chats_synced || state.selected_chat.is_some() || state.auto_selected {
                return;
            }
            state
                .overview_chats(Utc::now())
                .first()
                .map(|(_, c)| c.id.clone())
        };
        if let Some(first) = first {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(first), cx));
        }
    }

    /// Open a session from the sidebar: select it, the main area follows.
    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    /// `+` (sidebar header, or the titlebar while the sidebar is collapsed):
    /// open the new-session canvas. A set sidebar filter re-homes the canvas
    /// onto that project; under "All" the current pick (the last selected
    /// project, restored from composer defaults) stands.
    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        let target = {
            let state = self.state.read(cx);
            self.settings
                .space_filter
                .clone()
                .filter(|id| state.space_row(id).is_some())
        };
        self.state.update(cx, |s, cx| {
            if target.is_some() {
                s.select_space(target, cx);
            }
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    /// The unified titlebar in chat mode:
    /// `[fading +] [harness icon + session title] … [toggle-changes]`.
    /// Replaces the tab strip; inherits its titlebar duties (drag region,
    /// animated left inset, the toggle-changes button on git projects).
    pub(super) fn render_session_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // The canvas titles as NOTHING (user request — a "New session"
        // header over the empty canvas was noise); the bar keeps its height,
        // drag region, and buttons. A session appends its target as a muted
        // "project @ device" tag right of the title (the composer footer no
        // longer carries it).
        let (title, target, harness, cwd_path, on_canvas): (
            SharedString,
            Option<SharedString>,
            Option<zeron_proto::HarnessId>,
            Option<String>,
            bool,
        ) = {
            let state = self.state.read(cx);
            match state.selected_chat_row() {
                Some(chat) => {
                    let folder = chat
                        .space_id
                        .as_deref()
                        .and_then(|id| state.space_row(id))
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "~".to_string());
                    let device = state
                        .device_name(&chat.device_id)
                        .unwrap_or("Unknown device");
                    // Resolve the absolute path to open: worktree path,
                    // chat cwd, or the space's folder path.
                    let open_path = chat.cwd.clone().or_else(|| {
                        chat.space_id
                            .as_deref()
                            .and_then(|id| state.space_row(id))
                            .map(|s| s.path.clone())
                    });
                    (
                        SharedString::from(transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )),
                        Some(SharedString::from(format!("{folder} @ {device}"))),
                        chat.config.as_ref().map(|c| c.harness),
                        open_path,
                        false,
                    )
                }
                None => (SharedString::from(""), None, None, None, true),
            }
        };

        // The new-session `+` renders in the WINDOW-CONTROL CLUSTER while the
        // sidebar is collapsed (`render_titlebar_cluster`) — this row only
        // budgets for it: the title's left inset grows by one button slot as
        // the + fades in, so the text never sits under it.
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let plus_inset = 26.0 * self.titlebar_plus_alpha();

        // Same glide as the old strip: content starts at the inset card's
        // left edge while the sidebar is open, and slides toward the control
        // cluster as it collapses.
        let content_left =
            (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start() + plus_inset);

        // Trailing titlebar section. With the changes pane open this is the
        // PANE'S HEADER — a strip exactly as wide as the pane carrying its
        // controls (scope dropdown, ref selector, fold-all from the Changes
        // entity; expand + close shell-side). It lives up here because the
        // titlebar overlay owns this band's hit-testing: controls mounted in
        // the pane itself would sit under the drag region and never see a
        // click. Closed, it is just the stable open/close toggle. Hidden on
        // the new-session canvas (user request) — nothing to diff yet.
        let takeover = !on_canvas && self.right_pane_open(cx) && self.right_pane_expanded;
        // In takeover the title hides and the strip owns the whole band, so
        // the row's left inset pulls back to the sidebar seam — the title
        // inset would push the scope dropdown off the pane's own left gutter
        // (user report: misaligned dead space). With the sidebar COLLAPSED
        // the seam is the window edge, where the traffic lights + nav
        // cluster overlay lives — the strip must still clear it, but only
        // just: `title_bar_content_start` carries a 10px TEXT margin the
        // strip doesn't want (it brings its own 8px pad), and doubling up
        // read as a hole after the `+` (user report).
        let row_left = if takeover {
            // The surface tabs must LEFT-ALIGN with the pane's own rows (the
            // diff options and stats strip carry an 8px box gutter off the
            // seam — user report: rows started at different insets). The
            // strip's width is capped to `avail`, which subtracts the row's
            // 8px child gap — pulling row_left 8 LEFT of the seam cancels
            // that, so the uncapped strip starts exactly at the seam and its
            // own 8px pad lands the first chip on the pane gutter. The
            // window-control cluster still wins while the sidebar is
            // collapsed (the chips clear it instead of underlapping).
            let cluster_end = self.title_bar_content_start() - 10.0 + plus_inset - 14.0;
            (sidebar_now - 8.0).max(cluster_end)
        } else {
            content_left
        };
        let trailing: Option<gpui::AnyElement> = if on_canvas {
            None
        } else if self.right_pane_open(cx) {
            let right_now = self.eval_tween(self.right_tween, self.right_target(cx));
            let pr = self.titlebar_right_pad(Theme::SPACE_LG);
            // The row's own left padding is part of its content box: a strip
            // wider than what's left after it overflows and clips at the right
            // edge (flex_none never shrinks) — cap to the available width. The
            // row's 8px child gaps sit OUTSIDE the strip's width (one before
            // the strip in takeover, two with the title row present): without
            // budgeting them the capped strip overflows by exactly one gap and
            // the buttons slide right on expand (user report).
            let gap_budget = if takeover { 8.0 } else { 16.0 };
            let avail = self.viewport_width - row_left - pr - gap_budget;
            // The right pane's SURFACE TABS (t3 RightPanelTabs) — the diff
            // options that used to live here moved into the pane's own
            // second row; expand/close stay in this band (user request).
            let controls = self.render_right_tab_strip(cx);
            Some(
                div()
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .overflow_hidden()
                    // Right edge already sits at viewport − pr (the row's own
                    // padding), so this width starts the strip exactly at the
                    // pane's left border — and rides the open/close tween.
                    .w(px((right_now - pr).min(avail).max(0.0)))
                    // 8 + the trigger's own 8px pad = the pane's 16px text
                    // gutter, so the scope label sits flush over the stats
                    // strip below.
                    .pl(px(8.0))
                    // Clipped: a long base-ref name must truncate inside the
                    // controls, never paint under the buttons to the right.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(controls),
                    )
                    .child(header_icon_button(
                        "expand-changes",
                        icons::EXPAND_ARROWS,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane_expand(cx)),
                    ))
                    .child(header_icon_button(
                        "toggle-changes",
                        icons::SIDEBAR_MINIMALISTIC,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                    ))
                    .into_any_element(),
            )
        } else {
            Some(
                header_icon_button(
                    "toggle-changes",
                    icons::SIDEBAR_MINIMALISTIC,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                )
                .into_any_element(),
            )
        };

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(8.0))
            .pl(px(row_left))
            .pr(px(self.titlebar_right_pad(Theme::SPACE_LG)))
            // In panel takeover the header strip spans the whole band — the
            // title would sit UNDER it (both flex_none, the row overflows and
            // paint order stacks them), so it hides for the duration.
            .when(!takeover, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .when_some(
                            harness.map(crate::pickers::harness_brand_icon),
                            |el, (path, tint)| {
                                el.child(
                                    icon(path)
                                        .size(px(14.0))
                                        .flex_none()
                                        .text_color(tint.unwrap_or(theme.text_muted)),
                                )
                            },
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if on_canvas {
                                    theme.text_muted.opacity(0.7)
                                } else {
                                    theme.text.opacity(0.85)
                                })
                                .child(title),
                        )
                        .when_some(target, |el, target| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(target),
                            )
                        }),
                )
            })
            .child(div().flex_1())
            .when(!takeover, |el| {
                el.when_some(
                    cwd_path.map(|path| self.render_editor_control(path, &theme, cx)),
                    |el, control| el.child(control),
                )
            })
            .children(trailing);

        // The unified window titlebar: full-width on the glass shell, ABOVE
        // the inset card. No bottom border — the card's own hairline is the
        // separation; the glass gutter shows between.
        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-titlebar", bar, cx)
            .into_any_element()
    }
}
