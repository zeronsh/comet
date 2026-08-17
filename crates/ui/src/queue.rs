//! The pending-message queue, docked above the composer.
//!
//! Everything you typed while the agent was busy, in the order it will be sent.
//! The rows live on the session doc ([`zeron_doc::QueuedMessage`]), so the phone
//! shows the same queue and either device can reorder it.
//!
//! What a row can do is deliberately the whole set: move it (drag, or the
//! arrows for people who don't drag), retype it, send it now — which stops the
//! turn and hands it over — or drop it. Editing a row to nothing IS dropping
//! it: emptying the box you just filled is a clear enough statement that
//! "delete" would only be a second way to say it.

use gpui::{
    AnyElement, Context, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled, Window, div, prelude::*, px,
};

use zeron_doc::QueuedMessage;
use zeron_rpc::methods;

use crate::composer::Composer;
use crate::icons::{self, icon};
use crate::terminal::panel::drop_index;
use crate::theme::Theme;

/// One row's slot: a line of 12.5px copy with the breathing room a hover plate
/// needs to read as a plate, and no more. These rows sit between the transcript
/// and the draft, where vertical space costs the most.
const ROW_HEIGHT: f32 = 26.0;
/// A row's own horizontal inset, which is also the hover plate's overhang.
const ROW_PAD_X: f32 = 8.0;
/// The leading column every row opens with (its place in line).
const LEAD: f32 = 16.0;

/// A queue row being dragged (gpui drag-and-drop). Scoped to its chat so a
/// drag can't land in a queue it didn't come from.
pub struct QueueDragPayload {
    chat: String,
    from: usize,
}

/// Where the dragged row would land, tracked while it hovers the list.
pub struct QueueDragState {
    pub from: usize,
    pub over: usize,
}

/// The cursor ghost: the message, at the row's own size.
struct QueueGhost {
    text: SharedString,
}

impl Render for QueueGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(ROW_HEIGHT))
            .max_w(px(320.0))
            .px(px(ROW_PAD_X))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.5))
            .text_color(theme.text)
            .opacity(0.9)
            .child(div().truncate().child(self.text.clone()))
    }
}

/// One line of a queued message: the newlines that make it a paragraph in the
/// composer make it three rows here, and the row is one line tall.
fn one_line(text: &str) -> SharedString {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    SharedString::from(flat)
}

/// "3 queued" — the panel header's aside.
pub fn queue_label(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 queued".to_string()),
        n => Some(format!("{n} queued")),
    }
}

impl Composer {
    /// The queue panel, or `None` when nothing is waiting. Wears the composer's
    /// own pill chrome so the two read as one column.
    pub(crate) fn render_queue_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items = self.state.read(cx).queue.clone();
        let label = queue_label(items.len())?;
        let theme = Theme::of(cx).clone();
        let chat_id = self.state.read(cx).selected_chat.clone()?;
        let count = items.len();
        let drag_from = self.queue_drag.as_ref().map(|d| d.from);
        let editing = self.editing_queued.clone();

        let list_chat = chat_id.clone();
        let drop_chat = chat_id.clone();
        let rows = div()
            .flex()
            .flex_col()
            .on_drag_move::<QueueDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<QueueDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != list_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_y = f32::from(event.event.position.y) - f32::from(event.bounds.top());
                    let over = drop_index(rel_y, ROW_HEIGHT, count);
                    this.update_queue_drag_over(from, over, cx);
                },
            ))
            .on_drop::<QueueDragPayload>(cx.listener(
                move |this, payload: &QueueDragPayload, _, cx| {
                    if payload.chat != drop_chat {
                        this.queue_drag = None;
                        cx.notify();
                        return;
                    }
                    let to = this
                        .queue_drag
                        .as_ref()
                        .map(|d| d.over)
                        .unwrap_or(payload.from);
                    this.queue_drag = None;
                    this.move_queued(payload.from, to, cx);
                },
            ))
            .children(items.iter().enumerate().map(|(ix, item)| {
                self.queue_row(&chat_id, ix, item, count, drag_from, &editing, &theme, cx)
            }));

        Some(
            div()
                .rounded(px(18.0))
                .bg(theme.input_glass_bg())
                .border_1()
                .border_color(theme.border)
                .when(!theme.is_glass(), |el| el.shadow_lg())
                .px(px(8.0))
                .pt(px(6.0))
                .pb(px(8.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(20.0))
                        .px(px(ROW_PAD_X))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(SharedString::from(crate::popover::tracked_upper("Queued"))),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_muted.opacity(0.45))
                                .child(SharedString::from(label)),
                        ),
                )
                .child(rows)
                .into_any_element(),
        )
    }

    /// One queued message: its place in line, the text, and — on hover — the
    /// five things you can do to it.
    #[allow(clippy::too_many_arguments)]
    fn queue_row(
        &self,
        chat_id: &str,
        ix: usize,
        item: &QueuedMessage,
        count: usize,
        drag_from: Option<usize>,
        editing: &Option<String>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The dragged row leaves a gap behind; the cursor ghost is the row.
        if drag_from == Some(ix) {
            return div().h(px(ROW_HEIGHT)).flex_none().into_any_element();
        }
        let key = SharedString::from(format!("queue-{}", item.id));
        let group = SharedString::from(format!("{key}-grp"));
        let being_edited = editing.as_deref() == Some(item.id.as_str());
        let text = one_line(&item.text);
        let ghost_text = text.clone();

        let up = (ix > 0).then(|| {
            self.queue_action(
                &key,
                "up",
                icons::ARROW_UP,
                &group,
                theme,
                cx.listener(move |this, _, _, cx| {
                    this.move_queued(ix, ix - 1, cx);
                }),
            )
        });
        let down = (ix + 1 < count).then(|| {
            self.queue_action(
                &key,
                "down",
                icons::ARROW_DOWN,
                &group,
                theme,
                cx.listener(move |this, _, _, cx| {
                    this.move_queued(ix, ix + 1, cx);
                }),
            )
        });
        let edit_id = item.id.clone();
        let edit = self.queue_action(
            &key,
            "edit",
            icons::PEN,
            &group,
            theme,
            cx.listener(move |this, _, window, cx| {
                this.begin_queue_edit(edit_id.clone(), window, cx);
            }),
        );
        let now_id = item.id.clone();
        let send_now = self.queue_action(
            &key,
            "now",
            icons::ARROW_RIGHT,
            &group,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.send_queued_now(now_id.clone(), cx);
            }),
        );
        let drop_id = item.id.clone();
        let discard = self.queue_action(
            &key,
            "drop",
            icons::CLOSE,
            &group,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.remove_queued(drop_id.clone(), cx);
            }),
        );

        div()
            .id(SharedString::from(format!("{key}-row")))
            .group(group.clone())
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px(px(ROW_PAD_X))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(7.0))
            .when(being_edited, |el| el.bg(crate::theme::ink(0.06)))
            .when(!being_edited, |el| {
                el.hover(|s| s.bg(crate::theme::ink(0.04)))
            })
            .cursor(gpui::CursorStyle::Arrow)
            .on_drag(
                QueueDragPayload {
                    chat: chat_id.to_string(),
                    from: ix,
                },
                move |_payload, _point, _, cx| {
                    let text = ghost_text.clone();
                    cx.stop_propagation();
                    cx.new(|_| QueueGhost { text })
                },
            )
            .child(
                // Its place in line — the same key-cap chip the wizard's
                // option rows wear, at row scale.
                div()
                    .size(px(LEAD))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .bg(crate::theme::ink(0.05))
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(format!("{}", ix + 1))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(theme.text.opacity(0.9))
                    .child(text),
            )
            // Files are why a row can sit through a steerable turn, so say so.
            .when(!item.attachments.is_empty(), |el| {
                el.child(
                    crate::icons::icon(crate::icons::PAPERCLIP)
                        .size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.7)),
                )
            })
            .when(being_edited, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("{key}-cancel")))
                        .flex_none()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(5.0))
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.75))
                        .hover(|s| s.bg(crate::theme::ink(0.07)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_queue_edit(cx);
                        }))
                        .child("Editing below — cancel"),
                )
            })
            .when(!being_edited, |el| {
                el.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .children(up)
                        .children(down)
                        .child(edit)
                        .child(send_now)
                        .child(discard),
                )
            })
            .into_any_element()
    }

    /// One of a row's trailing glyph buttons — quiet until the row is hovered,
    /// so five affordances don't shout over the message they belong to.
    fn queue_action(
        &self,
        key: &SharedString,
        slot: &str,
        glyph: &'static str,
        group: &SharedString,
        theme: &Theme,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        // gpui paints an svg with the colour on the svg itself — a text colour
        // set on this button would never reach the glyph — so the reveal rides
        // the button's opacity and the pointer brightening rides its own group.
        let own = SharedString::from(format!("{key}-{slot}-grp"));
        div()
            .id(SharedString::from(format!("{key}-{slot}")))
            .group(own.clone())
            .size(px(18.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .opacity(0.0)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .hover(|s| s.bg(crate::theme::ink(0.07)))
            .on_click(on_click)
            .child(
                icon(glyph)
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .group_hover(own, |s| s.text_color(theme.text)),
            )
            .into_any_element()
    }

    /// Track the drop slot while a row is dragged over the list.
    fn update_queue_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.queue_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.over = over;
                    cx.notify();
                }
            }
            _ => {
                self.queue_drag = Some(QueueDragState { from, over });
                cx.notify();
            }
        }
    }

    /// Move the row at `from` to `to`, optimistically here and for real on the
    /// doc (the watch frame is what everyone else sees).
    pub(crate) fn move_queued(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to {
            cx.notify();
            return;
        }
        let Some(id) = self
            .state
            .read(cx)
            .queue
            .get(from)
            .map(|item| item.id.clone())
        else {
            return;
        };
        self.state.update(cx, |state, cx| {
            if from < state.queue.len() {
                let item = state.queue.remove(from);
                state.queue.insert(to.min(state.queue.len()), item);
                cx.notify();
            }
        });
        self.queue_rpc(
            methods::MOVE_QUEUED_MESSAGE,
            serde_json::json!({ "id": id, "toIndex": to }),
            "Couldn't reorder the queue",
            cx,
        );
    }

    /// Drop a queued message.
    pub(crate) fn remove_queued(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.editing_queued = None;
        }
        self.state.update(cx, |state, cx| {
            state.queue.retain(|item| item.id != id);
            cx.notify();
        });
        self.queue_rpc(
            methods::REMOVE_QUEUED_MESSAGE,
            serde_json::json!({ "id": id }),
            "Couldn't remove the message",
            cx,
        );
    }

    /// Send one now: the host stops the turn and hands this message over. Not
    /// optimistic — the row leaves the queue when the host has actually taken
    /// it, so a failed interrupt doesn't lose the text.
    pub(crate) fn send_queued_now(&mut self, id: String, cx: &mut Context<Self>) {
        if self.editing_queued.as_deref() == Some(id.as_str()) {
            self.editing_queued = None;
        }
        self.queue_rpc(
            methods::SEND_QUEUED_MESSAGE_NOW,
            serde_json::json!({ "id": id }),
            "Couldn't send that message",
            cx,
        );
    }

    /// Hitting Enter on an empty composer with a queue behind it: the top
    /// message goes now, interrupting the turn. The gesture only exists
    /// because that is what an empty Enter can mean — there is nothing to send
    /// but there is something waiting.
    pub(crate) fn queue_pop_head(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .state
            .read(cx)
            .queue
            .first()
            .map(|item| item.id.clone())
        else {
            return;
        };
        self.send_queued_now(id, cx);
    }

    /// Lift a queued message into the composer to retype it. The row stays in
    /// place (and stays in line) until the edit is committed.
    pub(crate) fn begin_queue_edit(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self
            .state
            .read(cx)
            .queue
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        // Whatever was half-typed is a draft of its own: park it so the edit
        // doesn't eat it (committing or cancelling puts it back).
        let typed = self.input.read(cx).text();
        self.queue_edit_stash = (!typed.trim().is_empty()).then(|| typed.to_string());
        self.editing_queued = Some(id);
        self.input
            .update(cx, |input, cx| input.set_text(item.text.clone(), cx));
        let focus = self.input.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Commit the edit in the box. Empty text removes the row — emptying a
    /// message is how you take it back. `true` when this consumed the submit.
    pub(crate) fn commit_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.editing_queued.take() else {
            return false;
        };
        let text = self.input.read(cx).text().trim().to_string();
        if text.is_empty() {
            self.remove_queued(id, cx);
        } else {
            self.state.update(cx, |state, cx| {
                if let Some(item) = state.queue.iter_mut().find(|item| item.id == id) {
                    item.text = text.clone();
                    cx.notify();
                }
            });
            self.queue_rpc(
                methods::UPDATE_QUEUED_MESSAGE,
                serde_json::json!({ "id": id, "text": text }),
                "Couldn't save that edit",
                cx,
            );
        }
        self.restore_queue_edit_stash(cx);
        true
    }

    /// Escape out of an edit, leaving the row as it was.
    pub(crate) fn cancel_queue_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.editing_queued.take().is_none() {
            return false;
        }
        self.restore_queue_edit_stash(cx);
        true
    }

    fn restore_queue_edit_stash(&mut self, cx: &mut Context<Self>) {
        let stash = self.queue_edit_stash.take().unwrap_or_default();
        self.input.update(cx, |input, cx| input.set_text(stash, cx));
        cx.notify();
    }

    /// Fire one queue mutation at the chat's doc host.
    fn queue_rpc(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let mut params = params;
        if let Some(object) = params.as_object_mut() {
            object.insert("chatId".into(), serde_json::Value::String(chat_id));
        }
        // Detached, not held: these are independent one-shot mutations, and
        // parking them in a single slot meant the next arrow tap dropped — and
        // so cancelled — the move still in flight, leaving the optimistic list
        // showing an order the doc never got.
        cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(method, params).await {
                tracing::warn!(method, error = %err, "queue mutation failed");
                this.update(cx, |composer, cx| {
                    composer.failure = Some(failure.into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{one_line, queue_label};

    #[test]
    fn label_counts_or_says_nothing() {
        assert_eq!(queue_label(0), None);
        assert_eq!(queue_label(1).as_deref(), Some("1 queued"));
        assert_eq!(queue_label(4).as_deref(), Some("4 queued"));
    }

    /// A row is one line tall, so a multi-line message has to read as one line
    /// — otherwise the panel's rows stop lining up.
    #[test]
    fn rows_flatten_multi_line_messages() {
        assert_eq!(
            one_line("fix the test\n\nthen ship it").as_ref(),
            "fix the test then ship it"
        );
        assert_eq!(one_line("  spaced   out  ").as_ref(), "spaced out");
    }
}
