//! Composer geometry and paint shared by native and browser callers.
//! No draft, submission, queue, attachment or transport policy lives here.
use crate::theme::Theme;
use gpui::{prelude::*, *};

pub const RADIUS: f32 = 26.0;
pub const MAX_WIDTH: f32 = 768.0;
pub fn container() -> Div {
    div()
        .w_full()
        .max_w(px(MAX_WIDTH))
        .mx_auto()
        .flex()
        .flex_col()
        .gap(px(Theme::SPACE_SM))
        .px(px(Theme::SPACE_LG))
        .pb(px(Theme::SPACE_LG))
}
pub fn pill(theme: &Theme) -> Div {
    div()
        .rounded(px(RADIUS))
        .bg(theme.input_glass_bg())
        .border_1()
        .border_color(theme.border)
        .when(!theme.is_frost(), |el| el.shadow_lg())
}
pub fn send_button(
    theme: &Theme,
    stop: bool,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(if stop {
            "composer-stop"
        } else {
            "composer-send"
        })
        .size(px(28.0))
        .flex_none()
        .rounded_full()
        .bg(theme.text)
        .flex()
        .items_center()
        .justify_center()
        .when(!enabled, |el| el.opacity(0.35))
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(on_click)
        })
        .child(if stop {
            div()
                .size(px(11.0))
                .rounded(px(3.0))
                .bg(theme.bg)
                .into_any_element()
        } else {
            crate::icons::icon(crate::icons::ARROW_UP)
                .size(px(14.0))
                .text_color(theme.bg)
                .into_any_element()
        })
        .into_any_element()
}
/// Native expanded branch. Callers provide measured dimensions and already-owned entities.
#[allow(clippy::too_many_arguments)]
pub fn expanded(
    pill: Div,
    pill_height: f32,
    textarea_height: f32,
    text_pt: f32,
    cluster_dy: f32,
    right_inset: f32,
    actions_height: f32,
    primary_gap: f32,
    utility_gap: f32,
    comments: Option<AnyElement>,
    strip: Option<AnyElement>,
    input: AnyElement,
    pickers: AnyElement,
    attach: Option<AnyElement>,
    send: AnyElement,
) -> Div {
    pill.h(px(pill_height))
        .overflow_hidden()
        .relative()
        .flex()
        .flex_col()
        .children(comments)
        .children(strip)
        .child(
            div()
                .h(px(textarea_height))
                .flex_none()
                .overflow_hidden()
                .px(px(16.0))
                .pt(px(text_pt))
                .pb(px(4.0))
                .child(input),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(-cluster_dy))
                .h(px(actions_height))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(primary_gap))
                .pl(px(12.0))
                .pr(px(right_inset))
                .pt(px(4.0))
                .pb(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(utility_gap))
                        .child(pickers)
                        .children(attach),
                )
                .child(send),
        )
}
