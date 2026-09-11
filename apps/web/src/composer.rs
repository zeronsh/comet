//! Actual shared input implementation; not the full native Composer wrapper.
use crate::{
    motion,
    settings::{ComposerSendBehavior, platform_combo},
    theme::Theme,
};
use gpui::{prelude::*, *};
use std::{collections::HashMap, ops::Range, path::PathBuf, rc::Rc, time::Duration};
use unicode_segmentation::UnicodeSegmentation;
use web_time::Instant;
const TEXTAREA_MAX: f32 = 260.0;
const TEXTAREA_PAD_V: f32 = 20.0;
include!("../../../crates/ui/src/composer/input.rs");

/// Settled native expanded layout; no compact/morph state or new draft owner.
pub fn prepare_expanded(
    input: &mut ComposerInput,
    window: &mut Window,
    cx: &mut Context<ComposerInput>,
) -> f32 {
    if input.needs_measure && input.last_width > 0.0 {
        let theme = Theme::of(cx);
        let mut style = window.text_style();
        style.font_family = theme.font_sans.clone();
        style.font_size = crate::typography::ui_rems(INPUT_TEXT_SIZE).into();
        style.color = if input.content.is_empty() {
            theme.text_faint
        } else {
            theme.text
        };
        input.layout_text(px(input.last_width), &style, window, cx);
    }
    let textarea = (input.measured_content_height() + TEXTAREA_PAD_V).clamp(76.0, TEXTAREA_MAX);
    let height = textarea - TEXTAREA_PAD_V;
    if input.viewport_height != Some(height)
        || input.settled_viewport_height != Some(height)
        || input.overflow_top_padding != 16.0
    {
        input.viewport_height = Some(height);
        input.settled_viewport_height = Some(height);
        input.overflow_top_padding = 16.0;
        cx.notify();
    }
    textarea
}

/// Discard credential editing history as well as visible text after exchange.
pub fn clear_secret(input: &mut ComposerInput, cx: &mut Context<ComposerInput>) {
    input.set_text("", cx);
    input.undo_stack.clear();
    input.redo_stack.clear();
}
