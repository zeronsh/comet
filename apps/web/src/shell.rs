#[path = "editor_owner.rs"]
mod editor_owner;
use crate::{
    composer::{self, ComposerInput, ComposerInputEvent},
    session::{AuthState, SessionController, SessionSnapshot, WriteOutcome},
    transcript::{Transcript, source::TranscriptDocument},
};
use crate::{
    composer_presentation as pill,
    icons::{self, icon},
    picker_presentation as chips, popover, shell_presentation as chrome,
    theme::Theme,
    typography::ui_rems,
};
use editor_owner::{EditorOwner, Owner};
use gpui::{prelude::*, *};
use std::rc::Rc;
use zeron_doc::{MessagePart, SessionMessageEntry};
use zeron_proto::HarnessId;

pub struct Shell {
    controller: Rc<SessionController>,
    state: SessionSnapshot,
    document: Entity<TranscriptDocument>,
    transcript: Entity<Transcript>,
    input: Entity<ComposerInput>,
    token: Entity<ComposerInput>,
    title: Entity<ComposerInput>,
    model_search: Entity<ComposerInput>,
    model_active: usize,
    model_scroll: UniformListScrollHandle,
    new_chat_open: bool,
    selected_space: Option<String>,
    browse: bool,
    projects_open: bool,
    sidebar_open: bool,
    policy_open: bool,
    sidebar_scroll: ScrollHandle,
    bottom_stack: Rc<std::cell::Cell<f32>>,
    local_error: Option<String>,
    inspected_unknown: bool,
    editor_owner: EditorOwner,
    auth_epoch: u64,
}

// Only geometry, status and counts are exposed. Never drafts, tokens, IDs or responses.
fn attr(name: &str, value: impl ToString) {
    if let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = root.set_attribute(&format!("data-{name}"), &value.to_string());
    }
}
fn marker(name: String) -> impl IntoElement {
    canvas(move |b, _, _| attr(&name, serde_json::json!({"x":f32::from(b.origin.x),"y":f32::from(b.origin.y),"width":f32::from(b.size.width),"height":f32::from(b.size.height)})), |_,_,_,_| {})
        .absolute().top_0().left_0().size_full()
}
fn field(name: &str, input: Entity<ComposerInput>) -> impl IntoElement {
    let focus_input = input.clone();
    div()
        .relative()
        .min_w_0()
        .w_full()
        .min_h(px(48.0))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&focus_input.read(cx).focus_handle(cx), cx);
        })
        .rounded_md()
        .bg(rgb(0x242428))
        .child(input)
        .child(marker(name.into()))
}
fn note(text: impl Into<SharedString>) -> Div {
    div().text_sm().text_color(rgb(0xa1a1aa)).child(text.into())
}

impl Shell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak = cx.entity().downgrade();
        let async_cx = cx.to_async();
        let controller = Rc::new(SessionController::new(move || {
            let weak = weak.clone();
            let mut async_cx = async_cx.clone();
            // Defer synchronous controller notifications past the active GPUI borrow.
            wasm_bindgen_futures::spawn_local(async move {
                let _ = weak.update(&mut async_cx, |this: &mut Self, cx| this.sync(cx));
            });
        }));
        let state = controller.snapshot();
        let document = cx.new(|_| TranscriptDocument::new("", Vec::new()));
        let transcript = cx.new(|cx| Transcript::from_document(document.clone(), true, cx));
        let input = cx.new(|cx| {
            ComposerInput::with_context("Message · Ctrl+Enter to send", "MessageComposer", cx)
        });
        let token =
            cx.new(|cx| ComposerInput::with_context("Paste gateway token", "MessageComposer", cx));
        let title = cx.new(|cx| {
            ComposerInput::with_context("New chat title (optional)", "MessageComposer", cx)
        });
        let model_search = cx.new(|cx| ComposerInput::new("Search models…", cx));
        cx.subscribe(&model_search, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.model_active = 0;
                this.model_scroll.scroll_to_item(0, ScrollStrategy::Nearest);
                cx.notify();
            }
        }).detach();
        window.focus(&token.read(cx).focus_handle(cx), cx);
        cx.subscribe(&input, |this, input, event, cx| {
            match event {
                ComposerInputEvent::Edited => {
                    this.update_draft(cx);
                }
                ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                    this.send(cx)
                }
                ComposerInputEvent::PastedImages(_) | ComposerInputEvent::PastedPaths(_) => {
                    this.local_error = Some(
                        "Text only: image and file uploads are not supported in this web client."
                            .into(),
                    );
                }
                _ => {}
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&token, |this, _, event, cx| {
            if matches!(
                event,
                ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted
            ) {
                this.login(cx);
            }
            cx.notify();
        })
        .detach();
        cx.observe(&transcript, |_, _, cx| cx.notify()).detach();
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        let start_controller = controller.clone();
        cx.defer(move |_| start_controller.start());
        Self {
            controller,
            state,
            document,
            transcript,
            input,
            token,
            title,
            model_search,
            model_active: 0,
            model_scroll: UniformListScrollHandle::new(),
            new_chat_open: false,
            selected_space: None,
            browse: false,
            projects_open: false,
            // GPUI's embedded window has zero bounds until its first frame.
            sidebar_open: web_sys::window().and_then(|w| w.inner_width().ok())
                .and_then(|w| w.as_f64()).is_none_or(|width| width >= 700.0),
            policy_open: false,
            sidebar_scroll: ScrollHandle::new(),
            bottom_stack: Rc::new(std::cell::Cell::new(160.0)),
            local_error: None,
            inspected_unknown: false,
            editor_owner: EditorOwner::default(),
            auth_epoch: 0,
        }
    }
    fn sync(&mut self, cx: &mut Context<Self>) {
        let next = self.controller.snapshot();
        if self.state.auth == AuthState::SignedIn && next.auth != AuthState::SignedIn {
            self.auth_epoch += 1;
        }
        let entries = if next.transcript_ready {
            next.transcript.clone()
        } else {
            Vec::new()
        };
        let id = next.selected_chat.clone().unwrap_or_default();
        let doc = self.document.read(cx);
        if doc.doc_id() != id || doc.entries() != entries {
            self.document
                .update(cx, |doc, cx| doc.replace(id, entries, cx));
        }
        if self
            .editor_owner
            .sync(self.owner_for(&next), next.draft_error.is_some())
            && self.input.read(cx).text() != next.draft
        {
            self.input
                .update(cx, |input, cx| input.set_text(next.draft.clone(), cx));
        }
        if self
            .selected_space
            .as_ref()
            .is_some_and(|id| !next.spaces.iter().any(|space| &space.id == id))
        {
            self.selected_space = None;
        }
        if next.selected_chat != self.state.selected_chat
            || !next.can_resolve_unknown_outcome
            || next.unknown_outcome_warning != self.state.unknown_outcome_warning
        {
            self.inspected_unknown = false;
        }
        self.state = next;
        cx.notify();
    }
    fn login(&mut self, cx: &mut Context<Self>) {
        if self.state.auth != AuthState::SignedOut {
            return;
        }
        let token = self.token.read(cx).text().trim().to_owned();
        self.token.update(cx, composer::clear_secret);
        if token.is_empty() {
            self.local_error =
                Some("Enter the token from the gateway's private token file.".into());
        } else {
            self.local_error = None;
            self.controller.login(token);
        }
        cx.notify();
    }
    fn owner_for(&self, state: &SessionSnapshot) -> Option<Owner> {
        if state.auth != AuthState::SignedIn {
            return None;
        }
        Some(Owner {
            auth_epoch: self.auth_epoch,
            engine: state.engine.as_ref()?.device_id.clone(),
            chat: state.selected_chat.clone()?,
        })
    }
    fn update_draft(&mut self, cx: &mut Context<Self>) -> bool {
        // Fresh controller state closes the gap before its deferred UI notification.
        if !self
            .editor_owner
            .can_write(&self.owner_for(&self.controller.snapshot()))
        {
            self.editor_owner.edited(false);
            self.local_error = Some("This retained buffer belongs to another chat or login. Copy it before explicitly discarding; it cannot be sent here.".into());
            return false;
        }
        let result = self
            .controller
            .set_draft(self.input.read(cx).text().to_owned());
        self.editor_owner.edited(result.is_ok());
        self.local_error = result.err();
        self.local_error.is_none()
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        if self.state.busy {
            self.local_error = Some("Queueing is unavailable in web. Draft retained.".into());
            cx.notify();
            return;
        }
        if !self.update_draft(cx) {
            cx.notify();
            return;
        }
        self.local_error = self.controller.send().err();
        // Do not clear here: only controller acknowledgment may clear the captured draft.
        cx.notify();
    }
    fn button(
        &self,
        id: String,
        label: String,
        enabled: bool,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        popover::menu_row(Theme::of(cx), false, SharedString::from(id.clone()))
            .id(SharedString::from(id.clone()))
            .relative()
            .min_w_0()
            .when(!enabled, |el| el.opacity(0.4))
            .child(div().truncate().child(label))
            .child(marker(id))
            .when(enabled, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
    }
    fn target_label(&self, chat: &zeron_proto::Chat) -> SharedString {
        let space = self
            .state
            .spaces
            .iter()
            .find(|s| Some(&s.id) == chat.space_id.as_ref())
            .map(|s| s.display_name().to_string())
            .unwrap_or_else(|| "~".into());
        let device = self
            .state
            .devices
            .iter()
            .find(|d| d.id == chat.device_id)
            .map(|d| d.name.as_str())
            .unwrap_or("Unknown device");
        format!("{space} @ {device}").into()
    }
    fn sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = Theme::of(cx);
        let s = &self.state;
        let label = self
            .selected_space
            .as_ref()
            .and_then(|id| s.spaces.iter().find(|s| &s.id == id))
            .map(|s| s.display_name().to_string())
            .unwrap_or_else(|| "All projects".into());
        let filter = chrome::project_trigger(self.projects_open, theme)
            .relative()
            .on_click(cx.listener(|this, _, _, cx| {
                this.projects_open = !this.projects_open;
                cx.notify();
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(marker("projects".into()));
        let mut chats: Vec<_> = s
            .chats
            .iter()
            .filter(|chat| {
                !chat.archived
                    && self
                        .selected_space
                        .as_ref()
                        .is_none_or(|space| chat.space_id.as_ref() == Some(space))
            })
            .collect();
        chats
            .sort_by_key(|chat| std::cmp::Reverse(chat.last_message_at.unwrap_or(chat.created_at)));
        let rows = chats.iter().enumerate().map(|(ix, chat)| {
            let id = chat.id.clone();
            let selected = s.selected_chat.as_ref() == Some(&id);
            let local = s.engine.as_ref().is_some_and(|engine| engine.device_id == chat.device_id);
            let fade = format!("chat-row-{id}");
            let selected_bg = crate::theme::glass_selected_bg();
            let bg = if selected { selected_bg } else { crate::theme::wash(0.0) };
            let hover = if selected { selected_bg } else { theme.glass_hover() };
            let status = s.sessions.iter().find(|run| run.chat_id == id).map(|run| run.status);
            let (label, color) = if !local { ("Remote unavailable".to_string(), theme.text_faint) } else {
                match status {
                    Some(zeron_proto::SessionStatus::Errored) => ("Failed".into(), theme.danger),
                    Some(zeron_proto::SessionStatus::Working) => ("Working".into(), theme.text_muted),
                    Some(zeron_proto::SessionStatus::AwaitingInput) => ("Input".into(), theme.warning),
                    _ => {
                        let minutes = (chrono::Utc::now() - chat.last_message_at.unwrap_or(chat.created_at)).num_minutes().max(0);
                        (if minutes < 60 {format!("{minutes}m")} else {format!("{}h", minutes / 60)}, theme.text_faint)
                    }
                }
            };
            let corner = div().text_size(ui_rems(10.0)).text_color(color).child(label).into_any_element();
            let brand = chat.config.as_ref().map(|c| chips::harness_brand_icon(c.harness)).map(|(path, tint)|
                icon(path).size(px(12.0)).flex_none().text_color(tint.unwrap_or(theme.text_muted).opacity(0.8)).into_any_element());
            chrome::chat_row(format!("chat-{id}").into(), &fade, if selected {theme.text} else {theme.text.opacity(0.8)}, theme.text, bg, hover)
                .relative().on_hover(crate::motion::hover_listener(fade))
                .when(s.ready && local, |el| el.cursor_pointer().on_click(cx.listener(move |this, _, window, cx| {
                    if this.editor_owner.rejected { this.local_error = Some("Correct or explicitly discard the retained edit before switching chats.".into()); cx.notify(); return; }
                    this.controller.select_chat(Some(id.clone())); this.browse = false; this.local_error = None;
                    if f32::from(window.viewport_size().width) < 700.0 { this.sidebar_open = false; }
                    window.focus(&this.input.read(cx).focus_handle(cx), cx); cx.notify();
                })))
                .child(chrome::chat_context(self.target_label(chat), corner, theme.text_muted.opacity(0.55)))
                .child(chrome::chat_title(chat.title.clone().unwrap_or_else(|| "New session".into()).into(), brand, Theme::SPACE_SM))
                .child(marker(format!("chat-{ix}")))
        });
        chrome::sidebar(256.0)
            .flex_none()
            .pt(px(Theme::TITLEBAR_HEIGHT))
            .relative()
            .child(
                div()
                    .flex_none()
                    .px(px(Theme::SPACE_SM))
                    .py(px(8.0))
                    .child(filter),
            )
            .when(self.projects_open, |el| {
                el.child(
                    popover::popover_card(theme)
                        .mx(px(8.0))
                        .max_h(px(220.0))
                        .id("project-menu")
                        .overflow_y_scroll()
                        .child(self.button(
                            "all-projects".into(),
                            "All projects".into(),
                            true,
                            cx,
                            |this, _, cx| {
                                this.selected_space = None;
                                this.projects_open = false;
                                cx.notify();
                            },
                        ))
                        .children(s.spaces.iter().enumerate().map(|(ix, space)| {
                            let id = space.id.clone();
                            let local = s
                                .engine
                                .as_ref()
                                .is_some_and(|engine| engine.device_id == space.device_id);
                            self.button(
                                format!("space-{ix}"),
                                format!(
                                    "{}{}",
                                    space.display_name(),
                                    if local { "" } else { " · remote unavailable" }
                                ),
                                s.ready && local,
                                cx,
                                move |this, _, cx| {
                                    this.selected_space = Some(id.clone());
                                    this.projects_open = false;
                                    cx.notify();
                                },
                            )
                        })),
                )
            })
            .child(
                crate::edge_fade::edge_faded(
                    24.0,
                    true,
                    true,
                    div().relative().flex_1().min_h_0().child(
                        chrome::session_list()
                            .track_scroll(&self.sidebar_scroll)
                            .child(div().flex().flex_col().gap(px(2.0)).children(rows))
                            .when(chats.is_empty(), |el| {
                                el.child(div().p_2().child(note("No sessions yet")))
                            }),
                    ),
                )
                .fade_overflow_y(&self.sidebar_scroll),
            )
            .child(
                div()
                    .p(px(Theme::SPACE_SM))
                    .flex_none()
                    .child(note(if s.ready {
                        "Local engine · Connected"
                    } else {
                        "Reconnecting to local engine…"
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(self.button(
                                "reconnect".into(),
                                "Reconnect".into(),
                                true,
                                cx,
                                |this, _, _| this.controller.reconnect(),
                            ))
                            .child(self.button(
                                "logout".into(),
                                "Sign out".into(),
                                true,
                                cx,
                                |this, _, _| this.controller.logout(),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn filtered_models(&self, cx: &App) -> Vec<zeron_proto::Model> {
        let query = self.model_search.read(cx).text().trim();
        let mut ranked: Vec<_> = self.state.models.iter().enumerate().filter_map(|(ix, model)| {
            chips::models::match_rank(query, model).map(|rank| (rank, ix, model.clone()))
        }).collect();
        ranked.sort_by_key(|(rank, ix, _)| (*rank, *ix));
        ranked.into_iter().map(|(_, _, model)| model).collect()
    }
    fn pick_model(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.ready || !self.editor_owner.can_write(&self.owner_for(&self.state)) { return; }
        let Some(harness) = self.state.selected_harness else { return; };
        let Some(model) = self.filtered_models(cx).get(ix).cloned() else { return; };
        self.controller.select_model(harness, model.id);
        self.browse = false;
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }
    fn catalog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.state;
        let owned_theme = Theme::of(cx).clone();
        let theme = &owned_theme;
        let locked = s.chats.iter().find(|c| Some(&c.id) == s.selected_chat.as_ref())
            .and_then(|c| c.config.as_ref()).map(|c| c.harness);
        let tabs = chips::models::tabs().children(s.harnesses.iter().enumerate().filter_map(|(ix, value)| {
            let harness: HarnessId = serde_json::from_value(value.get("id")?.clone()).ok()?;
            if harness == HarnessId::Mock { return None; }
            let enabled = s.ready && value.get("installed").and_then(|v|v.as_bool()) == Some(true)
                && value.get("enabled").and_then(|v|v.as_bool()).unwrap_or(true)
                && locked.is_none_or(|h| h == harness);
            Some(chips::models::harness_tab(("harness-tab",ix), harness, s.selected_harness == Some(harness), !enabled, theme)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if !enabled || !this.editor_owner.can_write(&this.owner_for(&this.state)) { return; }
                    this.controller.select_model(harness, String::new());
                    this.model_active = 0;
                    this.model_scroll.scroll_to_item(0, ScrollStrategy::Nearest);
                    window.focus(&this.model_search.read(cx).focus_handle(cx), cx);
                    cx.notify();
                })).child(marker(format!("harness-{ix}"))))
        }));
        let rows = self.filtered_models(cx);
        attr("picker-row-count", rows.len());
        attr("picker-active", self.model_active);
        let list: AnyElement = if !s.ready {
            div().p_2().child(popover::skeleton_menu_rows("model-skeleton", theme, 5, cx.entity_id(), cx)).into_any_element()
        } else if rows.is_empty() {
            chips::models::empty_list_note(theme, if self.model_search.read(cx).text().trim().is_empty() { "No models available" } else { "No models found" })
        } else {
            let entity = cx.entity();
            uniform_list("model-menu-scroll", rows.len(), move |range, _, app| {
                entity.update(app, |this, cx| {
                    let theme = Theme::of(cx);
                    range.filter_map(|ix| rows.get(ix).map(|model| {
                        let selected = this.state.selected_model.as_ref() == Some(&model.id);
                        let body = chips::models::compact_body(model.label.clone().into(), model.description.clone().map(Into::into), theme);
                        div().pb(px(2.0)).child(chips::models::row(ix, true, selected, ix == this.model_active)
                            .relative().on_hover(cx.listener(move |this, hovered: &bool, _, cx| { if *hovered && this.model_active != ix { this.model_active = ix; cx.notify(); } }))
                            .on_click(cx.listener(move |this, _, window, cx| this.pick_model(ix, window, cx)))
                            .child(body).child(marker(format!("model-{ix}")))).into_any_element()
                    })).collect::<Vec<AnyElement>>()
                })
            }).size_full().px(px(6.0)).track_scroll(&self.model_scroll).into_any_element()
        };
        let search = chips::models::search_row(self.model_search.clone(), theme).relative().child(marker("model-search".into()));
        popover::popover_card_flush(theme).id("catalog").relative().flex_none()
            .w(px(chips::models::WIDTH)).max_w_full().mx_auto()
            .track_focus(&self.model_search.read(cx).focus_handle(cx))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                use popover::MenuKey;
                let key = popover::classify_key(&event.keystroke.key, event.keystroke.modifiers.platform, event.keystroke.modifiers.control);
                match key {
                    MenuKey::Escape => { this.browse = false; window.focus(&this.input.read(cx).focus_handle(cx), cx); }
                    MenuKey::Up | MenuKey::Down => {
                        this.model_active = popover::menu_step(Some(this.model_active), this.filtered_models(cx).len(), if key == MenuKey::Up {-1} else {1}).unwrap_or(0);
                        this.model_scroll.scroll_to_item(this.model_active, ScrollStrategy::Nearest);
                    }
                    MenuKey::Enter => this.pick_model(this.model_active, window, cx),
                    _ => return,
                }
                cx.stop_propagation(); cx.notify();
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| { this.browse = false; cx.notify(); }))
            .child(chips::models::content(tabs, search, chips::models::list_host().child(list)))
            .child(marker("model-popup".into()))
    }
    fn new_chat_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        popover::popover_card(Theme::of(cx)).w(px(304.0)).max_w_full().mx_auto().flex().flex_col().gap_2()
            .child(note("New chat · choose a project in the sidebar"))
            .child(field("new-title", self.title.clone()))
            .child(self.button("new-chat".into(), "New local chat".into(), self.state.can_create_chat && self.selected_space.is_some(), cx, |this, _, cx| {
                if let Some(space) = this.selected_space.clone() {
                    if this.editor_owner.rejected { this.local_error = Some("Correct or explicitly discard the retained edit before creating another chat.".into()); cx.notify(); return; }
                    this.local_error = this.controller.create_chat(space, this.title.read(cx).text().to_owned()).err();
                    if this.local_error.is_none() { this.new_chat_open = false; }
                }
                cx.notify();
            }))
    }
    fn input_request(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.state.transcript_ready {
            return None;
        }
        let questions = self
            .state
            .transcript
            .iter()
            .flat_map(|entry| &entry.parts)
            .find_map(|part| match part {
                MessagePart::Input {
                    questions,
                    resolved: false,
                    ..
                } => Some(questions),
                _ => None,
            })?;
        Some(div().id("input-request").flex_none().max_h(px(160.0)).overflow_y_scroll()
            .p_3().flex().flex_col().gap_2().bg(rgb(0x292631))
            .child(note("Input response unavailable in web: the gateway blocks this action because the native turn policy is not safely constrained. Respond in the native app."))
            .children(questions.iter().map(|q| div().text_sm().child(q.question.clone())))
            .into_any_element())
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let textarea = self.input.update(cx, |input, cx| {
            composer::prepare_expanded(input, window, cx)
        });
        let stack_h = self.bottom_stack.get();
        self.transcript
            .update(cx, |t, cx| t.set_bottom_clearance(stack_h, cx));
        let s = &self.state;
        attr("presentation", "native-shared-expanded");
        attr("font-family", theme.font_sans.clone());
        attr("sidebar-open", self.sidebar_open);
        attr("auth", format!("{:?}", s.auth));
        attr("connection", format!("{:?}", s.connection));
        attr("ready", s.ready);
        attr("rows", self.transcript.read(cx).rows().len());
        attr("queue-count", s.queue.len());
        attr("can-send", s.can_send);
        attr("busy", s.busy);
        attr("chat-count", s.chats.len());
        attr("model-count", s.models.len());
        let mut root = chrome::root(&theme).flex_col();
        if s.auth != AuthState::SignedIn {
            let can_login = s.auth == AuthState::SignedOut;
            return root.child(div().id("login-panel").flex_1().min_h_0().overflow_y_scroll().p_4().flex().flex_col().gap_3()
                .child(div().text_xl().child(match s.auth { AuthState::Checking => "Checking session…", AuthState::SigningIn => "Signing in…", _ => "Connect to your local Comet" }))
                .child(note("Use the token in your gateway's private token file. It is exchanged for an HttpOnly session cookie and cleared from this editor."))
                .when(can_login, |el| el.child(field("token", self.token.clone())))
                .child(self.button("login".into(), "Sign in".into(), can_login, cx, |this,_,cx| this.login(cx)))
                .when(s.error.is_some(), |el| el.child(note("Sign-in or session check failed. Check the gateway and token, then retry.")))
                .children(self.local_error.clone().map(note))
                .child(note("Local text-only client. No uploads, remote attachment fetch, terminal or native settings parity."))).into_any_element();
        }
        root = div().id("web-bottom-stack").w_full().flex().flex_col();
        if self.editor_owner.rejected {
            root = root.child(div().flex_none().p_3().flex().flex_col().gap_2()
                .child(note("Retained edit: chat switching is blocked. Correct it in its original chat, or copy it before explicitly discarding. After an automatic context change this buffer cannot be sent to the new chat."))
                .child(self.button("discard-retained-edit".into(), "Discard retained edit (cannot undo)".into(), true, cx, |this,_,cx| {
                    let current = this.controller.snapshot();
                    this.editor_owner.discard();
                    this.local_error = None;
                    let _ = this.controller.set_draft(current.draft.clone());
                    this.sync(cx);
                })));
        }
        attr("model-picker-open", self.browse);
        if self.new_chat_open { root = root.child(self.new_chat_panel(cx)); }
        if self.browse {
            root = root.child(self.catalog(cx));
        }
        if let Some(error) = self.local_error.as_ref().or(s.error.as_ref()) {
            root = root.child(
                div()
                    .flex_none()
                    .px_3()
                    .text_sm()
                    .text_color(rgb(0xfca5a5))
                    .child(error.clone()),
            );
        }
        let outcome = match &s.outcome {
            Some(WriteOutcome::Pending) => Some("Request pending acknowledgment…"),
            Some(WriteOutcome::Accepted) => None,
            Some(WriteOutcome::UnknownOutcome) => {
                Some("Outcome unknown. Draft retained; check the transcript before retrying.")
            }
            Some(WriteOutcome::Rejected(_)) => Some("Request rejected. Draft retained."),
            None => None,
        };
        if let Some(text) = outcome {
            root = root.child(div().flex_none().px_3().child(note(text)));
        }
        if let Some(warning) = &s.unknown_outcome_warning {
            root = root.child(div().flex_none().p_3().flex().flex_col().gap_2()
                .child(note(warning.clone()))
                .child(note("Reconnect and inspect the refreshed chat and current state first. This does not retry the operation or clear your draft."))
                .child(note("This operation may already have happened; I inspected the current state."))
                .child(self.button("unknown-inspected".into(), format!("{} I confirm the statement above", if self.inspected_unknown { "[x]" } else { "[ ]" }), s.can_resolve_unknown_outcome, cx, |this,_,cx| { this.inspected_unknown = !this.inspected_unknown; cx.notify(); }))
                .child(self.button("resolve-unknown".into(), "Acknowledge uncertainty — unblock only".into(), s.can_resolve_unknown_outcome && self.inspected_unknown, cx, |this,_,cx| {
                    this.local_error = this.controller.resolve_unknown_outcome(this.inspected_unknown).err();
                    this.inspected_unknown = false;
                    cx.notify();
                })));
        }
        if self.policy_open {
            root = root.child(
                div()
                    .flex_none()
                    .px_3()
                    .child(note(s.policy_notice.clone())),
            );
        }
        let outlet = div()
            .size_full()
            .relative()
            .when(s.selected_chat.is_none(), |el| {
                el.child(div().p_4().pt(px(80.0)).child(note(
                    "Choose a session, or select a project and use + to create a chat.",
                )))
            })
            .when(s.selected_chat.is_some() && !s.transcript_ready, |el| {
                el.child(
                    div()
                        .p_4()
                        .pt(px(80.0))
                        .child(note("Loading or resynchronizing transcript…")),
                )
            })
            .when(s.selected_chat.is_some() && s.transcript_ready, |el| {
                // Document-backed transcripts use the desktop side-pane geometry:
                // their host, not their first row, owns titlebar clearance.
                el.child(div().absolute().inset_0()
                    .top(px(crate::transcript::OWN_SEND_TOP_INSET_PX))
                    .child(self.transcript.clone())
                    .child(marker("transcript-viewport".into())))
            });
        if let Some(panel) = self.input_request(cx) {
            root = root.child(panel);
        }
        let can_send = s.can_send && !s.busy && self.editor_owner.can_write(&self.owner_for(s));
        let stop = s.busy && s.can_interrupt;
        let send = div()
            .relative()
            .child(pill::send_button(
                &theme,
                stop,
                if stop { s.can_interrupt } else { can_send },
                cx.listener(move |this, _, window, cx| {
                    if stop {
                        this.local_error = this.controller.interrupt().err();
                        cx.notify();
                    } else {
                        this.send(cx);
                        window.focus(&this.input.read(cx).focus_handle(cx), cx);
                    }
                }),
            ))
            .child(marker(if stop { "stop" } else { "send" }.into()))
            .into_any_element();
        let model_label = s
            .selected_model
            .as_ref()
            .map(|id| {
                s.models
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| id.clone())
            })
            .unwrap_or_else(|| "Select model".into());
        let model = chips::trigger_chip(
            "picker-model",
            s.selected_model.is_some(),
            self.browse,
            &theme,
        )
        .relative()
        .on_click(cx.listener(|this, _, window, cx| {
            this.browse = !this.browse;
            this.new_chat_open = false;
            if this.browse {
                this.model_active = this.filtered_models(cx).iter().position(|m| Some(&m.id) == this.state.selected_model.as_ref()).unwrap_or(0);
                window.focus(&this.model_search.read(cx).focus_handle(cx), cx);
            }
            cx.notify();
        }))
        .children(
            s.selected_harness
                .map(chips::harness_brand_icon)
                .map(|(path, tint)| {
                    icon(path)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted))
                }),
        )
        .child(div().min_w_0().truncate().child(model_label))
        .child(marker("browse".into()))
        .into_any_element();
        let input = div()
            .relative()
            .min_w_0()
            .child(self.input.clone())
            .child(marker("composer".into()))
            .into_any_element();
        let body = pill::expanded(
            pill::pill(&theme),
            textarea + 48.0,
            textarea,
            16.0,
            0.0,
            12.0,
            46.0,
            Theme::SPACE_SM,
            2.0,
            None,
            None,
            input,
            model,
            None,
            send,
        );
        let composer = pill::container()
            .when(!s.queue.is_empty(), |el| el.child(div().id("pending-queue").max_h(px(90.0)).overflow_y_scroll()
                .child(note(format!("{} pending queued message(s) · read only", s.queue.len())))
                .children(s.queue.iter().map(|q| note(q.text.clone())))))
            .when(s.selected_chat.is_some() || self.editor_owner.rejected, |el| el.child(crate::frost::frosted(pill::RADIUS, 16.0, body)))
            .child(div().flex().items_center().gap(px(8.0)).px(px(8.0))
                .child(div().flex_1().min_w_0().text_size(ui_rems(11.0)).text_color(theme.text_faint)
                    .child("Provider runs disabled for this checkpoint"))
                .child(chips::trigger_chip("web-policy", false, self.policy_open, &theme)
                    .on_click(cx.listener(|this, _, _, cx| { this.policy_open = !this.policy_open; cx.notify(); }))
                    .child("Text only")))
            .when(self.policy_open, |el| el.child(note("Enter: newline · Ctrl+Enter: send. Authentication grants machine access, not a sandbox.")))
            .when(s.busy, |el| el.child(note("Busy: queueing is unavailable. Draft retained; interrupt the turn or wait.")));
        let measured = self.bottom_stack.clone();
        let weak = cx.entity().downgrade();
        let stack = root
            .child(composer)
            .relative()
            .flex_none()
            .max_h(px(
                (f32::from(window.viewport_size().height) - 70.0).max(180.0)
            ))
            .overflow_y_scroll()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let height = f32::from(bounds.size.height);
                        if (measured.get() - height).abs() > 0.5 {
                            measured.set(height);
                            let _ = weak.update(cx, |_, cx| cx.notify());
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );
        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .flex()
            .flex_col()
            .child(chrome::transcript_underlay(
                outlet.into_any_element(),
                0.0,
                stack_h,
            ))
            .child(div().flex_1().min_h_0())
            .child(stack);
        let sidebar_width = if self.sidebar_open { 256.0 } else { 0.0 };
        let selected = s
            .chats
            .iter()
            .find(|chat| Some(&chat.id) == s.selected_chat.as_ref());
        let title = selected
            .map(|chat| chat.title.clone().unwrap_or_else(|| "New session".into()))
            .unwrap_or_default();
        let target = selected.map(|chat| self.target_label(chat));
        let brand = selected
            .and_then(|chat| chat.config.as_ref())
            .map(|c| chips::harness_brand_icon(c.harness))
            .map(|(path, tint)| {
                icon(path)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(tint.unwrap_or(theme.text_muted))
                    .into_any_element()
            });
        let titlebar = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px((sidebar_width + Theme::SPACE_LG).max(132.0)))
                    .pr(px(16.0))
                    .child(chrome::title_identity(
                        &theme,
                        title.into(),
                        if f32::from(window.viewport_size().width) >= 700.0 {
                            target
                        } else {
                            None
                        },
                        brand,
                        selected.is_none(),
                    )),
            )
            .child(
                chrome::titlebar_cluster(10.0)
                    .child(
                        div()
                            .relative()
                            .child(chrome::window_control_button(
                                "toggle-sidebar",
                                icons::SIDEBAR_MINIMALISTIC_LEFT,
                                &theme,
                                cx.listener(|this, _, _, cx| {
                                    this.sidebar_open = !this.sidebar_open;
                                    cx.notify();
                                }),
                            ))
                            .child(marker("toggle-sidebar".into())),
                    )
                    .child(
                        div()
                            .ml(px(Theme::SPACE_SM))
                            .flex()
                            .gap(px(2.0))
                            .child(chrome::nav_history_button(
                                "nav-back",
                                icons::ARROW_LEFT,
                                false,
                                &theme,
                                |_, _, _| {},
                            ))
                            .child(chrome::nav_history_button(
                                "nav-forward",
                                icons::ARROW_RIGHT,
                                false,
                                &theme,
                                |_, _, _| {},
                            )),
                    )
                    .child(
                        div()
                            .ml(px(Theme::SPACE_SM))
                            .relative()
                            .child(chrome::window_control_button(
                                "titlebar-new-session",
                                icons::PLUS,
                                &theme,
                                cx.listener(|this, _, _, cx| {
                                    this.new_chat_open = !this.new_chat_open;
                                    this.browse = false;
                                    if this.selected_space.is_none() {
                                        this.sidebar_open = true;
                                        this.projects_open = true;
                                    }
                                    cx.notify();
                                }),
                            ))
                            .child(marker("new-session".into())),
                    ),
            );
        let root = chrome::root(&theme)
            .overflow_hidden()
            .child(chrome::sidebar_tone(sidebar_width, theme.border))
            .when(self.sidebar_open, |el| el.child(self.sidebar(cx)))
            // On a phone, browsing takes the conversation's place instead of
            // squeezing the native composer into a 134px column. Entities stay owned.
            .when(!self.sidebar_open || f32::from(window.viewport_size().width) >= 700.0,
                |el| el.child(main))
            .child(titlebar)
            .into_any_element();
        // As on desktop, tick after this frame's hover reads, then drive the
        // fade independently of unrelated caret or network notifications.
        if crate::motion::hover_fades_active() {
            window.request_animation_frame();
        }
        root
    }
}
