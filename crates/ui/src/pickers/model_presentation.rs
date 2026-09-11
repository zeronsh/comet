//! Actual desktop model-picker paint. Callers own catalogs, filtering and actions.
use crate::{theme::Theme, typography::ui_rems};
use gpui::{prelude::*, *};
use zeron_proto::HarnessId;

pub const WIDTH: f32 = 304.0;
pub const LIST_HEIGHT: f32 = 216.0;

pub fn tabs() -> Div {
    div()
        .flex_none()
        .h(px(40.0))
        .px(px(6.0))
        .border_b_1()
        .border_color(crate::theme::hairline(0.08))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
}
pub fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(ui_rems(12.0))
        .text_color(theme.text_muted.opacity(0.6))
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}
/// Native search ranking: model label first, provider attribution second.
pub fn match_rank(query: &str, model: &zeron_proto::Model) -> Option<usize> {
    let by_label = crate::popover::match_rank(query, &model.label);
    let by_description = crate::popover::match_rank(
        query,
        &format!(
            "{} {}",
            model.description.as_deref().unwrap_or(""),
            model.label
        ),
    )
    .map(|rank| rank + 2);
    by_label.into_iter().chain(by_description).min()
}

pub fn harness_tab(
    id: impl Into<ElementId>,
    harness: HarnessId,
    viewed: bool,
    disabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let (path, tint) = super::harness_brand_icon(harness);
    div()
        .id(id)
        .relative()
        .w(px(32.0))
        .h(px(32.0))
        .rounded(px(8.0))
        .flex()
        .items_center()
        .justify_center()
        .when(disabled, |el| el.opacity(0.35))
        .when(!disabled, |el| el.cursor_pointer())
        .when(!disabled && !viewed, |el| {
            el.hover(|s| s.bg(crate::theme::ink(0.06)))
        })
        .child(
            crate::icons::icon(path)
                .size(px(16.0))
                .text_color(tint.unwrap_or(if viewed { theme.text } else { theme.text_muted })),
        )
        .when(viewed, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom(px(-4.0))
                    .left(px(6.0))
                    .right(px(6.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(theme.accent),
            )
        })
}
pub fn search_row(input: impl IntoElement, theme: &Theme) -> Div {
    div()
        .flex_none()
        .h(px(40.0))
        .px(px(10.0))
        .border_b_1()
        .border_color(crate::theme::hairline(0.08))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            crate::icons::icon(crate::icons::MAGNIFER)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted.opacity(0.7)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(ui_rems(13.0))
                .child(input),
        )
}
pub fn list_host() -> Stateful<Div> {
    div()
        .id("model-list-scroll-host")
        .relative()
        .flex_none()
        .h(px(LIST_HEIGHT))
        .py(px(6.0))
        .bg(crate::theme::ink(0.02))
}
pub fn content(tabs: impl IntoElement, search: impl IntoElement, list: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .child(tabs)
        .child(search)
        .child(list)
}
pub fn row(ix: usize, compact: bool, selected: bool, active: bool) -> Stateful<Div> {
    div()
        .id(("model-row", ix))
        .px(px(8.0))
        .py(px(if compact { 5.0 } else { 6.0 }))
        .rounded(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .cursor_pointer()
        .when(selected, |el| {
            el.bg(crate::theme::card_selected_bg())
                .shadow(crate::theme::card_selected_shadows())
        })
        .when(!selected && active, |el| el.bg(crate::theme::ink(0.05)))
}
pub fn compact_body(
    label: SharedString,
    attribution: Option<SharedString>,
    theme: &Theme,
) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_none()
                .max_w_full()
                .truncate()
                .text_size(ui_rems(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(label),
        )
        .when_some(attribution, |el, attribution| {
            el.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(attribution),
            )
        })
        .into_any_element()
}
