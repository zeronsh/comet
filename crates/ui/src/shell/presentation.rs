//! Native shell presentation. Callers retain navigation, transport and mutation policy.
use crate::{icons::icon, motion, theme::Theme, typography::ui_rems};
use gpui::{prelude::*, *};

pub fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}
pub fn transcript_underlay(outlet: AnyElement, term_h: f32, stack_h: f32) -> Div {
    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
    div().absolute().inset_0().bottom(px(term_h)).child(
        crate::edge_fade::edge_faded(
            Theme::TRANSCRIPT_FADE_BAND,
            true,
            true,
            div().size_full().child(outlet),
        )
        .inset_top(Theme::TITLEBAR_HEIGHT)
        .band_top(Theme::TRANSCRIPT_FADE_BAND)
        .band_bottom(bottom_band),
    )
}

pub fn root(theme: &Theme) -> Stateful<Div> {
    div()
        .id("shell-root")
        .relative()
        .flex()
        .flex_row()
        .size_full()
        .bg(theme.glass())
        .text_color(theme.text)
        .font_family(theme.font_sans.clone())
        .text_size(ui_rems(14.0))
}
pub fn sidebar_tone(width: f32, border: Hsla) -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .w(px(width))
        .bg(crate::theme::wash(0.05))
        .border_r_1()
        .border_color(border)
}
pub fn sidebar(width: f32) -> Div {
    div().w(px(width)).h_full().flex().flex_col()
}
pub fn session_list() -> Stateful<Div> {
    div()
        .id("sidebar-lists")
        .size_full()
        .overflow_y_scroll()
        .px(px(Theme::SPACE_SM))
        .flex()
        .flex_col()
        .pt(px(4.0))
}
pub fn chat_row(
    id: SharedString,
    fade_key: &str,
    rest_text: Hsla,
    text: Hsla,
    rest_bg: Hsla,
    hover_bg: Hsla,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(2.0))
        .rounded(px(8.0))
        .px(px(Theme::SPACE_SM))
        .py(px(6.0))
        .text_color(motion::hover_blend(fade_key, rest_text, text))
        .bg(motion::hover_blend(fade_key, rest_bg, hover_bg))
}
pub fn chat_context(label: SharedString, corner: AnyElement, subline: Hsla) -> Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(Theme::SPACE_SM))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui_rems(11.0))
                .line_height(px(14.0))
                .text_color(subline)
                .child(label),
        )
        .child(div().text_color(subline).child(corner))
}
pub fn chat_title(title: SharedString, brand: Option<AnyElement>, gap: f32) -> Div {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(gap))
        .children(brand)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui_rems(13.0))
                .line_height(px(17.0))
                .child(title),
        )
}
pub fn project_trigger(open: bool, theme: &Theme) -> Stateful<Div> {
    div()
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
        .text_size(ui_rems(13.0))
        .font_weight(FontWeight::MEDIUM)
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
}
pub fn title_identity(
    theme: &Theme,
    title: SharedString,
    target: Option<SharedString>,
    brand: Option<AnyElement>,
    on_canvas: bool,
) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .children(brand)
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui_rems(12.0))
                .font_weight(FontWeight::MEDIUM)
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
                    .text_size(ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(target),
            )
        })
}
pub fn titlebar_cluster(pad: f32) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .h(px(Theme::TITLEBAR_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .pt(px(Theme::TITLEBAR_TOP_PAD))
        .px(px(pad))
}
pub fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Exclude controls from the native titlebar drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}
