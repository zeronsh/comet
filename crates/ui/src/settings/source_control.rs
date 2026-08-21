//! Settings → Source Control: local provider and authentication status.

use gpui::{AnyElement, Context, Entity, Render, SharedString, Task, Window, div, prelude::*, px};

use zeron_proto::SourceControlConnection;
use zeron_rpc::methods;

use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

pub struct SourceControlPage {
    state: Entity<AppState>,
    connections: Loadable<Vec<SourceControlConnection>>,
    load_task: Option<Task<()>>,
}

impl SourceControlPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            connections: Loadable::Idle,
            load_task: None,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.connections = Loadable::Error("Engine not connected".into());
            return;
        };
        self.connections = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_SOURCE_CONTROL_CONNECTIONS,
                    serde_json::json!({}),
                )
                .await;
            this.update(cx, |page, cx| {
                page.connections = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|err| Loadable::Error(err.to_string())),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn provider_row(connection: &SourceControlConnection, ix: usize, theme: &Theme) -> AnyElement {
        let status = if connection.connected {
            theme.success
        } else {
            theme.text_muted.opacity(0.35)
        };
        let account = connection
            .account
            .as_deref()
            .map(|account| format!("Authenticated as {account}"))
            .unwrap_or_else(|| connection.detail.clone());
        widgets::card_row(theme, ix == 0)
            .child(
                div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.glass_hover().opacity(0.35))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        connection
                            .name
                            .chars()
                            .take(2)
                            .collect::<String>()
                            .to_uppercase(),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(widgets::row_title(theme, connection.name.clone()))
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(account))
                                .into_any_element(),
                            connection
                                .cli_version
                                .clone()
                                .map(|version| {
                                    div()
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(version))
                                        .into_any_element()
                                })
                                .unwrap_or_else(|| div().into_any_element()),
                        ],
                    )),
            )
            .child(div().size(px(7.0)).rounded_full().bg(status))
            .into_any_element()
    }

    fn bitbucket_hint(theme: &Theme) -> gpui::Div {
        div()
            .mt(px(12.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(8.0))
            .bg(theme.glass_hover().opacity(0.35))
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(SharedString::from(
                "Bitbucket on macOS: run `launchctl setenv BITBUCKET_EMAIL \"you@example.com\"` and `launchctl setenv BITBUCKET_API_TOKEN \"your-token\"`, then fully quit and reopen Zeron.",
            ))
    }
}

impl Render for SourceControlPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let body: AnyElement = match &self.connections {
            Loadable::Idle | Loadable::Loading => widgets::section_card(&theme)
                .p(px(16.0))
                .child(crate::popover::skeleton_rows(
                    "source-control-skeleton",
                    &theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                widgets::error_strip(&theme, message.clone()).into_any_element()
            }
            Loadable::Ready(connections) => {
                let card = widgets::section_card(&theme).children(
                    connections
                        .iter()
                        .enumerate()
                        .map(|(ix, connection)| Self::provider_row(connection, ix, &theme)),
                );
                if connections
                    .iter()
                    .any(|connection| connection.provider == "bitbucket")
                {
                    card.child(Self::bitbucket_hint(&theme)).into_any_element()
                } else {
                    card.into_any_element()
                }
            }
        };

        div()
            .id("source-control-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .child(widgets::page_header(&theme, "Source Control", None))
                            .child(div().flex_1())
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("source-control-refresh")
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .on_click(cx.listener(|page, _, _, cx| page.load(cx)))
                                    .child(SharedString::from("Refresh")),
                            ),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "GitHub uses the authenticated gh CLI. Bitbucket uses BITBUCKET_API_TOKEN and BITBUCKET_EMAIL from the app environment.",
                    ))
                    .child(body),
            )
    }
}
