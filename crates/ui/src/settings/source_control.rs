//! Settings → Source control: how Zeron links to the pull requests behind a
//! checkout. The badges themselves stay provider-neutral
//! (`change_requests::pull_request_badge`); this page only picks which review
//! tool a GitHub link lands in.
//!
//! The NotificationsPage arrangement: the page holds a working copy, every
//! pick emits [`SourceControlEvent`], and the shell persists it. Nothing here
//! talks RPC — the link target is a device-local UI setting.

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use super::{PullRequestLinkTarget, widgets};
use crate::{icons, theme::Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceControlEvent {
    PullRequestLinkTargetChanged(PullRequestLinkTarget),
}

pub struct SourceControlPage {
    pull_request_link_target: PullRequestLinkTarget,
}

impl EventEmitter<SourceControlEvent> for SourceControlPage {}

impl SourceControlPage {
    pub fn new(pull_request_link_target: PullRequestLinkTarget, _cx: &mut Context<Self>) -> Self {
        Self {
            pull_request_link_target,
        }
    }

    fn select_pull_request_link_target(
        &mut self,
        target: PullRequestLinkTarget,
        cx: &mut Context<Self>,
    ) {
        if self.pull_request_link_target == target {
            return;
        }
        self.pull_request_link_target = target;
        cx.emit(SourceControlEvent::PullRequestLinkTargetChanged(target));
        cx.notify();
    }
}

impl Render for SourceControlPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let selected = self.pull_request_link_target;
        // The Files page's option pills, so the two pages read alike.
        let options = PullRequestLinkTarget::ALL.into_iter().map(|target| {
            let active = target == selected;
            div()
                .id(SharedString::from(format!(
                    "source-control-pull-request-{}",
                    target.label().to_ascii_lowercase()
                )))
                .h(px(28.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(if active {
                    theme.accent.opacity(0.7)
                } else {
                    theme.border
                })
                .bg(if active {
                    theme.accent.opacity(0.11)
                } else {
                    crate::theme::wash(0.025)
                })
                .text_size(px(11.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::wash(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_pull_request_link_target(target, cx);
                }))
                .child(target.label())
        });

        let card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .items_start()
                .child(widgets::row_tile(&theme, icons::PULL_REQUEST))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Open pull requests in"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                // Full width so the sentence wraps instead of
                                // running past the card at narrow widths.
                                div()
                                    .w_full()
                                    .child(SharedString::from(
                                        "Applies to the pull request badges in the sidebar \
                                         and composer. Linear and Graphite only rewrite \
                                         GitHub pull request links.",
                                    ))
                                    .into_any_element(),
                            ],
                        ))
                        .child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .flex_wrap()
                                .gap(px(7.0))
                                .children(options),
                        ),
                ),
        );

        div()
            .id("source-control-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Source control", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "How Zeron links to the pull requests behind your checkouts.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(card),
            )
    }
}
