//! Picker chrome without catalog or selection ownership.
use crate::{motion, theme::Theme, typography::ui_rems};
use gpui::{prelude::*, *};
use zeron_proto::HarnessId;

#[path = "model_presentation.rs"]
pub mod models;

pub fn trigger_chip(id: &'static str, set: bool, open: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.0))
        .max_w(px(248.0))
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(10.0))
        .rounded(px(8.0))
        .text_size(ui_rems(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(motion::hover_blend(
            id,
            if set {
                theme.text.opacity(0.9)
            } else {
                theme.text_muted
            },
            theme.text,
        ))
        .bg(if open {
            theme.element_hover
        } else {
            motion::hover_blend(id, transparent_black(), theme.element_hover)
        })
        .on_hover(motion::hover_listener(id))
        .cursor_pointer()
}
pub fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<Hsla>) {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
        HarnessId::Devin => (crate::icons::DEVIN_MARK, None),
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        HarnessId::Hermes => (crate::icons::HERMES_MARK, None),
        HarnessId::Pi => (crate::icons::PI_MARK, None),
        HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
    }
}
