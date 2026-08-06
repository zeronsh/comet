//! Spaces sidebar: the space-filter dropdown (searchable, with "All spaces"),
//! the filtered Sessions list, and the add-space palette (⌘K-style: device
//! tabs + filtered folder browser).
//!
//! A space = a synced (device, folder) pair. Spaces stopped being a
//! navigation spine when tabs went device-local: the dropdown only FILTERS
//! the sidebar's session list (never the tab strip) and hosts space
//! management (add via the palette; rename/delete via row context menus).
//! Child module of `shell` so it renders straight off `Shell`'s private state.

use super::*;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;

/// The space-filter dropdown, `Some` while open. The same searchable-menu
/// recipe as the composer's ref picker: filter input on top
/// (`PaletteSearch` context so ↑↓/⏎ bubble to the card), ranked substring
/// rows, keyboard highlight.
pub(super) struct SpacesMenu {
    search: Entity<ComposerInput>,
    /// Keyboard highlight within [`Shell::spaces_menu_rows`].
    active: usize,
    /// Tracked on the card — puts it on the keyboard dispatch path while the
    /// search input holds focus (the structure every working picker uses).
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

/// One row of the open dropdown, in display order.
#[derive(Clone, PartialEq)]
pub(super) enum SpacesMenuRow {
    All,
    Space(String),
    AddSpace,
}

/// The add-space palette (a command-K surface, summoned by ⌘K): search bar
/// across the top, folder browser on the left, a Devices rail on the right,
/// kbd-hint footer. One surface — picking a device in the rail rebrowses in
/// place, no step wizard.
pub(super) struct AddSpaceFlow {
    /// The device currently browsed (the highlighted rail row).
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the device-name crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED folder rows.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    _search_events: Subscription,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            theme.busy.opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            theme.success.opacity(0.9) // emerald-400
        }
        ChatIndicator::Idle => crate::theme::ink(0.14),
    }
}

impl Shell {
    // ---- space filter ----

    /// Set the sidebar's session filter (`None` = All spaces). On the
    /// new-session canvas the space context follows the filter — the canvas
    /// default is "the space you're looking at".
    pub(super) fn set_space_filter(&mut self, filter: Option<String>, cx: &mut Context<Self>) {
        self.settings.space_filter = filter.clone();
        if let Some(space_id) = filter
            && self.state.read(cx).selected_chat.is_none()
        {
            self.state
                .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        }
        self.close_spaces_menu(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Close the space-filter dropdown through the exit animation (no-op when
    /// it isn't open). Every close path funnels here so the menu always
    /// animates out instead of vanishing.
    fn close_spaces_menu(&mut self, cx: &mut Context<Self>) {
        if self.spaces_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.spaces_menu);
            cx.notify();
        }
    }

    /// Land in a just-added space: filter the sidebar to it and open the
    /// new-session canvas there.
    pub(super) fn land_in_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.settings.space_filter = Some(space_id.clone());
        self.settings.last_space_id = Some(space_id.clone());
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id), cx);
            s.select_chat(None, cx);
        });
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- sidebar sections ----

    /// The filter's display rows: "All spaces", then spaces matching the
    /// search (ranked — `popover::filter_indices`), then "Add space…".
    /// "All" only shows on an empty query (searching means hunting a space).
    fn spaces_menu_rows(&self, cx: &App) -> Vec<SpacesMenuRow> {
        let query = self
            .spaces_menu
            .get()
            .map(|menu| menu.search.read(cx).text().to_string())
            .unwrap_or_default();
        let state = self.state.read(cx);
        let spaces = state.spaces_sorted();
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        let mut rows: Vec<SpacesMenuRow> = Vec::new();
        if query.trim().is_empty() {
            rows.push(SpacesMenuRow::All);
        }
        rows.extend(
            popover::filter_indices(&query, &names)
                .into_iter()
                .map(|ix| SpacesMenuRow::Space(spaces[ix].id.clone())),
        );
        rows.push(SpacesMenuRow::AddSpace);
        rows
    }

    fn open_spaces_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // "PaletteSearch" context: ↑↓/⏎ stay unbound in the input and bubble
        // to the card's key handler.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search spaces…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(menu) = this.spaces_menu.open_mut() {
                    menu.active = 0;
                }
                cx.notify();
            }
        });
        // The highlight starts ON the current filter row.
        let current = self.settings.space_filter.clone();
        let handle = search.read(cx).focus_handle(cx);
        self.spaces_menu.open(SpacesMenu {
            search,
            active: 0,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            _search_events: search_events,
        });
        let rows = self.spaces_menu_rows(cx);
        let start = match &current {
            None => 0,
            Some(id) => rows
                .iter()
                .position(|row| matches!(row, SpacesMenuRow::Space(s) if s == id))
                .unwrap_or(0),
        };
        if let Some(menu) = self.spaces_menu.open_mut() {
            menu.active = start;
        }
        // Focusable before first paint (the add-space palette's proven order).
        window.focus(&handle, cx);
        cx.notify();
    }

    fn activate_spaces_menu_row(&mut self, row: SpacesMenuRow, cx: &mut Context<Self>) {
        match row {
            SpacesMenuRow::All => self.set_space_filter(None, cx),
            SpacesMenuRow::Space(id) => self.set_space_filter(Some(id), cx),
            SpacesMenuRow::AddSpace => {
                self.close_spaces_menu(cx);
                self.open_add_space(cx);
            }
        }
    }

    /// Dropdown keys (bubbling from the focused search input): ↑↓ navigate,
    /// ⏎ activates the highlighted row, esc closes.
    fn spaces_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // The card stays mounted (and focused) through the exit animation —
        // keys must not drive a dying menu.
        if !self.spaces_menu.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.close_spaces_menu(cx);
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.spaces_menu_rows(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.spaces_menu.open_mut() {
                    menu.active = popover::menu_step(Some(menu.active), count, delta).unwrap_or(0);
                    menu.list_scroll.scroll_to_item(menu.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let row = {
                    let active = self.spaces_menu.get().map(|m| m.active).unwrap_or(0);
                    self.spaces_menu_rows(cx).get(active).cloned()
                };
                if let Some(row) = row {
                    self.activate_spaces_menu_row(row, cx);
                }
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }

    /// The sidebar's space-filter row: current filter ("All spaces" or the
    /// space's name) + chevron, the dropdown floating beneath while open.
    /// Sits OUTSIDE the sidebar's scroll region so the float never clips.
    pub(super) fn render_spaces_filter(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filter = self.settings.space_filter.clone();
        // Name + the dropdown rows' "@ device" tag on the trigger itself, so
        // the filtered space's host reads without opening the picker.
        let (label, device_tag): (SharedString, Option<(SharedString, bool)>) = {
            let state = self.state.read(cx);
            match filter.as_deref().and_then(|id| state.space_row(id)) {
                Some(space) => {
                    let (tag, offline) = state.space_device_tag(space, Utc::now());
                    (
                        space.display_name().to_string().into(),
                        Some((tag.into(), offline)),
                    )
                }
                None => (SharedString::from("All spaces"), None),
            }
        };
        let open = self.spaces_menu.is_open();

        let trigger = div()
            .id("spaces-filter")
            .flex_1()
            .min_w_0()
            .h(px(29.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                "spaces-filter",
                theme.text.opacity(0.8),
                theme.text,
            ))
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "spaces-filter",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(motion::hover_listener("spaces-filter"))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                // The menu's `on_mouse_down_out` already closed it on this
                // click's mouse-down (the trigger is outside the card), so by
                // the time the click lands the menu reads as closed — without
                // the guard, clicking the open trigger would close-and-reopen.
                let just_dismissed = this
                    .spaces_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.spaces_menu_dismissed_at = None;
                if this.spaces_menu.is_open() {
                    this.close_spaces_menu(cx);
                } else if !just_dismissed {
                    this.open_spaces_menu(window, cx);
                }
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            // flex_1 pushes the caret to the trigger's right edge and gives
            // long space names a bound to truncate against; the "@ device"
            // tag hugs the name inside it rather than sitting by the caret.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().min_w_0().truncate().child(label))
                    .when_some(device_tag, |el, (tag, offline)| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::NORMAL)
                                .text_color(if offline {
                                    theme.warning.opacity(0.8)
                                } else {
                                    theme.text_muted.opacity(0.45)
                                })
                                .child(tag),
                        )
                    }),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            );
        let trigger = if self.spaces_menu.get().is_some() {
            let closing = self.spaces_menu.closing_since();
            let menu = self.render_spaces_menu(theme, cx);
            trigger.relative().child(popover::anchored_menu_below(
                "spaces-filter-menu",
                menu,
                closing,
            ))
        } else {
            trigger
        };

        // Quick add-space button beside the trigger (the old section header's
        // `+`, relocated).
        let add = div()
            .id("add-space")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "add-space",
                crate::theme::wash(0.0),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener("add-space"))
            .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
            .child(
                icon(icons::PLUS)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(trigger)
            .child(add)
            .into_any_element()
    }

    /// The dropdown card: search on top, "All spaces" + space rows (check on
    /// the active filter; right-click for rename/remove) + "Add space…".
    fn render_spaces_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (search, active, focus, list_scroll) = {
            let Some(menu) = self.spaces_menu.get() else {
                return div().into_any_element();
            };
            (
                menu.search.clone(),
                menu.active,
                menu.focus.clone(),
                menu.list_scroll.clone(),
            )
        };
        let rows = self.spaces_menu_rows(cx);
        let filter = self.settings.space_filter.clone();
        let now = Utc::now();
        // (name, device tag) per space row — presence reuses the session
        // rows' heartbeat signal.
        let details: Vec<(SpacesMenuRow, SharedString, Option<SharedString>, bool)> = {
            let state = self.state.read(cx);
            rows.iter()
                .map(|row| match row {
                    SpacesMenuRow::All => {
                        (row.clone(), SharedString::from("All spaces"), None, false)
                    }
                    SpacesMenuRow::Space(id) => match state.space_row(id) {
                        Some(space) => {
                            let (tag, offline) = state.space_device_tag(space, now);
                            (
                                row.clone(),
                                space.display_name().to_string().into(),
                                Some(tag.into()),
                                offline,
                            )
                        }
                        None => (row.clone(), SharedString::from("?"), None, false),
                    },
                    SpacesMenuRow::AddSpace => {
                        (row.clone(), SharedString::from("Add space…"), None, false)
                    }
                })
                .collect()
        };

        let list = div()
            .id("spaces-menu-list")
            .flex()
            .flex_col()
            .gap(px(2.0))
            .max_h(px(224.0))
            .overflow_y_scroll()
            .track_scroll(&list_scroll)
            .children(
                details
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (row, label, tag, offline))| {
                        let is_selected = match &row {
                            SpacesMenuRow::All => filter.is_none(),
                            SpacesMenuRow::Space(id) => filter.as_deref() == Some(id.as_str()),
                            SpacesMenuRow::AddSpace => false,
                        };
                        let leading = match &row {
                            SpacesMenuRow::AddSpace => icons::PLUS,
                            _ => icons::FOLDER,
                        };
                        let menu_space = match &row {
                            SpacesMenuRow::Space(id) => Some(id.clone()),
                            _ => None,
                        };
                        let activate = row.clone();
                        popover::menu_row_nav(
                            theme,
                            is_selected,
                            ix == active,
                            format!("spaces-menu-row-{ix}"),
                        )
                        .id(("spaces-menu-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.activate_spaces_menu_row(activate.clone(), cx);
                        }))
                        .when_some(menu_space, |el, space_id| {
                            el.on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.space_menu.open((space_id.clone(), event.position));
                                    cx.notify();
                                }),
                            )
                        })
                        .child(
                            icon(leading)
                                .size(px(15.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.8)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(label))
                        .when_some(tag, |el, tag| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.0))
                                    .text_color(if offline {
                                        theme.warning.opacity(0.8)
                                    } else {
                                        theme.text_muted.opacity(0.45)
                                    })
                                    .child(tag),
                            )
                        })
                        // No check glyph — the selected row's wash (menu_row's
                        // active styling) is the selection signal.
                    }),
            );

        popover::popover_card(theme)
            .w(px(248.0))
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.spaces_menu_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.spaces_menu_dismissed_at = Some(std::time::Instant::now());
                this.close_spaces_menu(cx);
            }))
            .flex()
            .flex_col()
            .child(popover::search_input_frame(
                theme,
                search.into_any_element(),
            ))
            .child(list)
            .into_any_element()
    }

    /// The sidebar's Sessions list: every session (idle included) of the
    /// filter space — or all spaces under "All" — attention-sorted. Rows are
    /// keyed for the FLIP resort glide.
    pub(super) fn render_active_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let filter = self.settings.space_filter.clone();
        let rows: Vec<(ChatIndicator, comet_proto::Chat, String, Option<String>)> = {
            let state = self.state.read(cx);
            state
                .overview_chats(now)
                .into_iter()
                .filter(|(_, chat)| match &filter {
                    Some(space_id) => chat.space_id.as_deref() == Some(space_id.as_str()),
                    None => true,
                })
                .map(|(status, chat)| {
                    let space = state.space_for_chat(chat);
                    let mut folder = space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    // Unknown device → no fragment, same as the archived list.
                    if let Some(device) = state.device_name(&chat.device_id) {
                        folder = format!("{folder}@{device}");
                    }
                    // The branch shows whenever the engine has stamped one —
                    // main-checkout sessions included, not just worktrees.
                    let branch = chat
                        .branch
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                        .map(str::to_string);
                    (status, chat.clone(), folder, branch)
                })
                .collect()
        };
        let selected = self.state.read(cx).selected_chat.clone();
        rows.into_iter()
            .map(|(status, chat, folder, branch)| {
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let is_selected = selected.as_deref() == Some(chat.id.as_str());
                let height = super::CHAT_ROW_HEIGHT;
                let harness = chat.config.as_ref().map(|c| c.harness);
                let element = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )
                    .into(),
                    time_ago,
                    folder.into(),
                    branch.map(SharedString::from),
                    harness,
                    status,
                    is_selected,
                    theme,
                    cx,
                );
                (format!("c:{}", chat.id), height, element)
            })
            .collect()
    }

    // ---- add-space flow (the ⌘K palette) ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        let devices: Vec<Device> = self.state.read(cx).devices.clone();
        let local = self.state.read(cx).local_device_id.clone();
        // Land on this device's tab (else the first registered device).
        let device = devices
            .iter()
            .find(|d| local.as_deref() == Some(d.id.as_str()))
            .or_else(|| devices.first())
            .cloned();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_space_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search folders…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
        });
        let has_device = device.is_some();
        self.add_space = Some(AddSpaceFlow {
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            _search_events: search_events,
        });
        if has_device {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    /// Devices-rail click: rebrowse the same palette on another device.
    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.as_ref().is_some_and(|d| d.id == device.id) {
            return;
        }
        flow.device = Some(device);
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.home = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(None, cx);
        cx.notify();
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// ListFolders on the flow's device (relay-forwarded when remote).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            // Only target remote devices — local calls skip the relay.
            if let (Some(target), local) = (&device_id, &local)
                && local.as_deref() != Some(target.as_str())
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => {
                                // A pathless browse resolved home — remember it
                                // so the breadcrumbs can fold it into the
                                // device crumb.
                                if went_home {
                                    flow.home = Some(listing.path.clone());
                                }
                                Loadable::Ready(listing)
                            }
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the space for the browser's current folder.
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        // Same (device, folder) already has a space → just switch to it. The
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if let Some(existing) = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| s.device_id == device.id && s.path == path)
            .map(|s| s.id.clone())
        {
            self.add_space = None;
            self.land_in_space(existing, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = space_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_space = None;
                        shell.land_in_space(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_space
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_space.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_space_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key: ↑↓ navigate, →/⏎ open the highlighted folder,
    /// ← up a level, ⌘⏎ add the OPEN folder, ⌫ (empty query) also goes up,
    /// esc closes.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_space_open_active(cx);
                return;
            }
            "left" => {
                self.add_space_go_up(cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_space = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.add_space_filtered(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // ⏎ opens the highlighted folder (an alias for →); the space is
            // added with ⌘⏎ — and the chord acts on the folder OPEN in the
            // breadcrumbs, not the highlight. The highlight auto-rests on the
            // first row, so a chord that took it would add arbitrary
            // subfolders; the usual target (a repo root full of subfolders)
            // is only ever "the folder you're standing in".
            popover::MenuKey::Enter => self.add_space_open_active(cx),
            popover::MenuKey::ModEnter => self.submit_add_space(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_space_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: ⌘K search bar (with the ⌘⏎ add / esc chips) ·
    /// breadcrumbs + folder list beside the devices rail · kbd-hint footer.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_space.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (
            device,
            search,
            error,
            submit_busy,
            active,
            loading,
            load_error,
            listing,
            focus,
            list_scroll,
            home,
        ) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.device.clone(),
                flow.search.clone(),
                flow.error.clone(),
                flow.submit_busy,
                flow.active,
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.focus.clone(),
                flow.list_scroll.clone(),
                flow.home.clone(),
            )
        };
        let devices = self.state.read(cx).devices.clone();
        let rows = self.add_space_filtered(cx);
        let query_empty = search.read(cx).is_empty();
        let hairline = crate::theme::hairline(0.06);
        let now = Utc::now();
        // (browsed device name, online) per rail row — presence is the same
        // signal the sidebar space rows use.
        let device_presence: Vec<bool> = {
            let state = self.state.read(cx);
            devices
                .iter()
                .map(|d| state.device_online(&d.id, now))
                .collect()
        };
        let device_name: SharedString = device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "This device".to_string())
            .into();

        // A quiet mono key-cap chip ("⌘K" / "esc") for the search bar ends.
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(crate::theme::ink(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };

        // ── search bar (the ⌘K bar): summon chip · input · "⌘ Enter" add ·
        //    esc. The primary chip leads with the ⌘ glyph, then says "Enter"
        //    in words (user request — the bare return arrow read as noise).
        let submit_chip = popover::btn_primary(&theme, "")
            .id("add-space-submit")
            .h(px(22.0))
            .px(px(8.0))
            .py(px(0.0))
            // Match the key-cap chips beside it (rounded-5) — btn_primary's
            // rounded-8 at this size read as a different component.
            .rounded(px(5.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| this.submit_add_space(cx)))
            .when(!submit_busy, |el| {
                el.child(
                    icon(icons::COMMAND)
                        .size(px(11.0))
                        .text_color(theme.on_solid.opacity(0.8)),
                )
                .child(SharedString::from("Enter"))
            })
            .when(submit_busy, |el| el.child(SharedString::from("Adding…")));
        // Header and footer sit a shade DEEPER than the body (the shared
        // recessed-band tone) — the bands frame the folder list, which stays
        // on the brighter tint.
        let band = popover::band();
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(
                key_chip(&theme)
                    .child(
                        icon(icons::COMMAND)
                            .size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(SharedString::from("K")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            )
            .child(submit_chip)
            .child(
                key_chip(&theme)
                    .id("add-space-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        // ── breadcrumbs ("MacBook Pro / Projects / comet"): the quiet mono
        //    path voice, `/` separators. The device crumb stands in for home —
        //    everything up to the resolved home path folds into it; below
        //    home the full path shows. Ancestors (device crumb included) are
        //    clickable.
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                // Root "/" chip always folds; the home segments fold too when
                // the browsed path sits at/under home.
                let at_home = home.as_deref() == Some(listing.path.as_str());
                let folded = 1 + home
                    .as_deref()
                    .filter(|h| listing.path == *h || listing.path.starts_with(&format!("{h}/")))
                    .map(|h| h.split('/').filter(|s| !s.is_empty()).count())
                    .unwrap_or(0);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(13.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .child({
                        let crumb = div()
                            .id("add-space-crumb-device")
                            .px(px(3.0))
                            .rounded(px(4.0))
                            .child(device_name.clone());
                        if at_home {
                            // Standing at home — the device crumb IS the
                            // current folder.
                            crumb
                                .text_color(theme.text.opacity(0.85))
                                .into_any_element()
                        } else {
                            crumb
                                .text_color(theme.text_muted.opacity(0.55))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(flow) = this.add_space.as_mut() {
                                        flow.browser_repo = false;
                                    }
                                    this.load_space_folders(None, cx);
                                }))
                                .into_any_element()
                        }
                    })
                    .children(segments.into_iter().enumerate().skip(folded).map(
                        |(ix, (label, full))| {
                            let is_last = ix == last;
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .text_color(theme.text_faint.opacity(0.7))
                                        .child(SharedString::from("/")),
                                )
                                .child({
                                    let crumb = div()
                                        .id(("add-space-crumb", ix))
                                        .px(px(3.0))
                                        .rounded(px(4.0))
                                        .text_color(if is_last {
                                            theme.text.opacity(0.85)
                                        } else {
                                            theme.text_muted.opacity(0.55)
                                        })
                                        .child(SharedString::from(label));
                                    if is_last {
                                        crumb.into_any_element()
                                    } else {
                                        crumb
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(theme.text))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(flow) = this.add_space.as_mut() {
                                                    flow.browser_repo = false;
                                                }
                                                this.load_space_folders(Some(full.clone()), cx);
                                            }))
                                            .into_any_element()
                                    }
                                })
                        },
                    ))
                    .into_any_element()
            }
            None => div().pt(px(6.0)).into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows(
                    "add-space-skeleton",
                    &theme,
                    6,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else if let Some(message) = load_error {
            let device_line = device
                .as_ref()
                .map(|d| format!("{} didn't respond — is it online?", d.name))
                .unwrap_or(message);
            popover::error_row(&theme, &device_line)
                .px(px(14.0))
                .py(px(10.0))
                .child(
                    div()
                        .id("add-space-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            // The 6px gutters live on a WRAPPER, outside the scroll viewport:
            // in-content padding/spacers can't do it — the wheel's max offset
            // eats bottom padding, and `scroll_to_item` (keyboard) pins the
            // row's bottom to the viewport edge regardless.
            div()
                .flex_1()
                .min_h_0()
                .py(px(6.0))
                .child(
                    div()
                        .id("add-space-folders")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&list_scroll)
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                        .gap(px(2.0))
                        .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                            let name: SharedString = entry.name.clone().into();
                            let full = crate::pickers::child_path(&base_path, &entry.name);
                            let is_repo = entry.is_repo;
                            popover::menu_row_nav(
                                &theme,
                                false,
                                ix == active,
                                format!("add-space-folder-{ix}"),
                            )
                            // The floating-card selection language: the wash
                            // plus the ring-only inset outline.
                            .when(ix == active, |el| {
                                el.shadow(crate::theme::card_selected_shadows())
                            })
                            .id(("add-space-folder", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_space_descend(full.clone(), is_repo, cx);
                            }))
                            .child(
                                icon(icons::FOLDER)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.8)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            // Repos get a quiet trailing branch glyph — the row
                            // you're usually hunting for announces itself.
                            .when(is_repo, |el| {
                                el.child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .flex_none()
                                        .text_color(theme.text_muted.opacity(0.5)),
                                )
                            })
                        })),
                )
                .into_any_element()
        };

        // ── devices rail (mock right column): platform glyph + name +
        //    presence dot per row, an info line naming the browsed device.
        //    Rows are the tab recipe (h-28 rounded-8 washes), vertical.
        let rail = div()
            .w(px(196.0))
            .flex_none()
            .border_l_1()
            .border_color(hairline)
            .px(px(8.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Devices")),
            )
            .children(devices.into_iter().enumerate().map(|(ix, dev)| {
                let is_active = device.as_ref().is_some_and(|d| d.id == dev.id);
                let online = device_presence.get(ix).copied().unwrap_or(false);
                // The Devices-page platform mapping (settings::devices).
                let platform_icon = match dev.platform.as_str() {
                    "macos" | "darwin" => icons::LAPTOP,
                    "web" => icons::GLOBAL,
                    "ios" | "android" => icons::SMARTPHONE,
                    _ => icons::MONITOR,
                };
                let name: SharedString = dev.name.clone().into();
                let pick = dev.clone();
                div()
                    .id(("add-space-device", ix))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        // The floating-card selection language: wash +
                        // ring-only inset outline.
                        el.bg(crate::theme::card_selected_bg())
                            .shadow(crate::theme::card_selected_shadows())
                            .text_color(theme.text)
                    })
                    .when(!is_active, |el| {
                        el.text_color(theme.text_muted.opacity(0.7))
                            .hover(|s| s.bg(theme.element_hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_space_pick_device(pick.clone(), cx);
                    }))
                    .child(
                        icon(platform_icon)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .size(px(5.0))
                            .rounded_full()
                            .flex_none()
                            .when(online, |el| {
                                // The Devices-page presence emerald, soft glow
                                // included.
                                let emerald = theme.success;
                                el.bg(emerald.opacity(0.9)).shadow(vec![gpui::BoxShadow {
                                    color: emerald.opacity(0.55),
                                    offset: gpui::point(px(0.0), px(0.0)),
                                    blur_radius: px(6.0),
                                    spread_radius: px(0.0),
                                    inset: false,
                                }])
                            })
                            .when(!online, |el| el.bg(crate::theme::ink(0.22))),
                    )
            }))
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(12.0))
                            .flex_none()
                            .mt(px(1.0))
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().child(SharedString::from(format!(
                        "Showing folders from {device_name} only"
                    )))),
            );

        // ── body: folder column (crumbs + list) beside the devices rail.
        //    FIXED height — sparse folders, loading skeletons, and device
        //    switches must not resize the card (the list fills and scrolls).
        let body = div()
            .h(px(330.0))
            .flex()
            .flex_row()
            .items_stretch()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(crumbs)
                    .child(list),
            )
            .child(rail);

        // ── footer: the shared key-cap legend voice (popover::key_hint).
        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(popover::key_hint_pair(
                &theme,
                icons::ARROW_UP,
                icons::ARROW_DOWN,
                "Navigate",
            ))
            .child(popover::key_hint(&theme, icons::ARROW_LEFT, "Up"))
            .child(popover::key_hint(&theme, icons::ARROW_RIGHT, "Open"))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            });

        let card =
            div()
                .id("add-space-palette")
                .w(px(680.0))
                .rounded(px(14.0))
                .border_1()
                .border_color(crate::theme::hairline(0.10))
                // The popover_card glass recipe: a translucent tint over the
                // frosted backdrop blur (`popover::modal` wraps in `frosted`) —
                // an opaque fill here killed the vibrancy every other float has.
                .bg(if theme.is_glass() {
                    theme.glass_overlay()
                } else {
                    theme.surface_overlay
                })
                .shadow_lg()
                .overflow_hidden()
                .flex()
                .flex_col()
                .text_color(theme.text)
                // On the keyboard dispatch path (see `AddSpaceFlow::focus`) — the
                // pickers' proven structure for frame-level keys with a focused
                // child input.
                .track_focus(&focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    this.add_space_key(event, cx)
                }))
                // Clicking the scrim dismisses (user requirement) — same close
                // path as Escape.
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.add_space = None;
                    cx.notify();
                }))
                .child(input_row)
                .child(body)
                .child(footer)
                .into_any_element();
        Some(popover::modal("add-space-dialog", viewport, card))
    }

    // ---- space context menu / rename / delete overlays ----

    fn close_space_menu(&mut self, cx: &mut Context<Self>) {
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.space_menu);
            cx.notify();
        }
    }

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.close_space_menu(cx);
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Space name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.get().cloned() {
            let closing = self.space_menu.closing_since();
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_space_menu(cx);
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_space_menu(cx);
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at(
                "space-context-menu",
                position,
                menu,
                closing,
            ));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename space"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this space".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove space?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}
