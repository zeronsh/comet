//! Repository-wide pull requests from the configured source-control providers.

use gpui::{
    AnyElement, Context, Entity, Render, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};

use zeron_proto::{
    ChangeRequestState, PullRequestChecksState, PullRequestListItem, PullRequestReviewState,
    SourceControlConnection,
};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::markdown::{parse_full, render as markdown_render};
use crate::popover::{Loadable, Popup};
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    Open,
    All,
    Closed,
    Merged,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InvolvementFilter {
    All,
    Reviewing,
    Authored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DraftFilter {
    All,
    DraftsOnly,
    HideDrafts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewFilter {
    All,
    Approved,
    ChangesRequested,
    Required,
    None,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChecksFilter {
    All,
    Passing,
    Failing,
    Pending,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostFilter {
    All,
    Github,
    Bitbucket,
}

#[derive(Clone, PartialEq, Eq)]
enum ProjectFilter {
    All,
    Repository(String),
}

#[derive(Clone, PartialEq, Eq)]
struct PullRequestFilters {
    state: StateFilter,
    involvement: InvolvementFilter,
    draft: DraftFilter,
    review: ReviewFilter,
    checks: ChecksFilter,
    host: HostFilter,
    project: ProjectFilter,
}

impl Default for PullRequestFilters {
    fn default() -> Self {
        Self {
            state: StateFilter::Open,
            involvement: InvolvementFilter::All,
            draft: DraftFilter::All,
            review: ReviewFilter::All,
            checks: ChecksFilter::All,
            host: HostFilter::All,
            project: ProjectFilter::All,
        }
    }
}

struct PullRequestRow {
    item: PullRequestListItem,
}

pub struct PullRequestsPage {
    state: Entity<AppState>,
    search: Entity<ComposerInput>,
    pull_requests: Loadable<Vec<PullRequestListItem>>,
    authored_accounts: Vec<(String, String)>,
    accounts_loaded: bool,
    selected: Option<PullRequestListItem>,
    filters: PullRequestFilters,
    filter_menu: Popup<()>,
    load_task: Option<Task<()>>,
    _search_events: Subscription,
}

#[derive(Clone)]
pub(crate) enum PullRequestsEvent {
    SelectionChanged(Option<PullRequestListItem>),
}

impl gpui::EventEmitter<PullRequestsEvent> for PullRequestsPage {}

impl PullRequestsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search pull requests, or label:bug", cx));
        let events = cx.subscribe(&search, |page, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                cx.notify();
            }
            let _ = page;
        });
        let mut page = Self {
            state,
            search,
            pull_requests: Loadable::Idle,
            authored_accounts: Vec::new(),
            accounts_loaded: false,
            selected: None,
            filters: PullRequestFilters::default(),
            filter_menu: Popup::default(),
            load_task: None,
            _search_events: events,
        };
        page.load(cx, false);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>, refresh: bool) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.pull_requests = Loadable::Error("Engine not connected".into());
            cx.notify();
            return;
        };
        let authored = self.filters.involvement == InvolvementFilter::Authored;
        let state = match self.filters.state {
            StateFilter::Open => "open",
            StateFilter::All => "all",
            StateFilter::Closed => "closed",
            StateFilter::Merged => "merged",
        };
        let host = match self.filters.host {
            HostFilter::All => None,
            HostFilter::Github => Some("github"),
            HostFilter::Bitbucket => Some("bitbucket"),
        };
        let project = match &self.filters.project {
            ProjectFilter::All => None,
            ProjectFilter::Repository(repository) => Some(repository.clone()),
        };
        let load_connections = !self.accounts_loaded;
        self.accounts_loaded = true;
        self.pull_requests = Loadable::Loading;
        cx.notify();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let connection_engine = engine.clone();
            let connection_call = async move {
                if load_connections {
                    Some(
                        connection_engine
                            .client()
                            .call(
                                methods::LIST_SOURCE_CONTROL_CONNECTIONS,
                                serde_json::json!({}),
                            )
                            .await,
                    )
                } else {
                    None
                }
            };
            let (result, connections) = tokio::join!(
                engine.client().call(
                    methods::LIST_PULL_REQUESTS,
                    serde_json::json!({
                        "state": state,
                        "authored": authored,
                        "host": host,
                        "project": project,
                        "refresh": refresh,
                    }),
                ),
                connection_call,
            );
            this.update(cx, |page, cx| {
                page.pull_requests = match result {
                    Ok(value) => serde_json::from_value(value)
                        .map(Loadable::Ready)
                        .unwrap_or_else(|err| Loadable::Error(err.to_string())),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let (Some(selected), Loadable::Ready(items)) =
                    (page.selected.as_ref(), &page.pull_requests)
                {
                    let still_visible = items.iter().any(|item| {
                        item.provider == selected.provider
                            && item.repository == selected.repository
                            && item.number == selected.number
                    });
                    if !still_visible {
                        page.selected = None;
                        cx.emit(PullRequestsEvent::SelectionChanged(None));
                    }
                }
                if let Some(connections) = connections {
                    page.authored_accounts = connections
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<Vec<SourceControlConnection>>(value).ok()
                        })
                        .map(|connections| {
                            connections
                                .into_iter()
                                .filter_map(|connection| {
                                    connection
                                        .account
                                        .map(|account| (connection.provider, account))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                }
                page.load_task = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn close_filter_menu(&mut self, cx: &mut Context<Self>) {
        if self.filter_menu.begin_close() {
            crate::popover::reap_popup(cx, |page: &mut Self| &mut page.filter_menu);
        }
    }

    fn toggle_filter_menu(&mut self, cx: &mut Context<Self>) {
        if self.filter_menu.take_press_was_open() || self.filter_menu.is_open() {
            self.close_filter_menu(cx);
        } else {
            self.filter_menu.open(());
            cx.notify();
        }
    }

    fn set_involvement(&mut self, value: InvolvementFilter, cx: &mut Context<Self>) {
        if self.filters.involvement != value {
            self.filters.involvement = value;
            self.close_filter_menu(cx);
            self.load(cx, false);
        }
    }

    fn set_state(&mut self, value: StateFilter, cx: &mut Context<Self>) {
        self.filters.state = value;
        self.close_filter_menu(cx);
        self.load(cx, false);
    }

    fn set_draft(&mut self, value: DraftFilter, cx: &mut Context<Self>) {
        self.filters.draft = value;
        self.close_filter_menu(cx);
        cx.notify();
    }

    fn set_review(&mut self, value: ReviewFilter, cx: &mut Context<Self>) {
        self.filters.review = value;
        self.close_filter_menu(cx);
        cx.notify();
    }

    fn set_checks(&mut self, value: ChecksFilter, cx: &mut Context<Self>) {
        self.filters.checks = value;
        self.close_filter_menu(cx);
        cx.notify();
    }

    fn set_host(&mut self, value: HostFilter, cx: &mut Context<Self>) {
        self.filters.host = value;
        self.close_filter_menu(cx);
        self.load(cx, false);
    }

    fn set_project(&mut self, value: ProjectFilter, cx: &mut Context<Self>) {
        self.filters.project = value;
        self.close_filter_menu(cx);
        self.load(cx, false);
    }

    fn rows(&self, items: &[PullRequestListItem], query: &str) -> Vec<PullRequestRow> {
        items
            .iter()
            .filter(|item| self.matches_filter(item, query))
            .cloned()
            .map(|item| PullRequestRow { item })
            .collect()
    }

    fn matches_filter(&self, item: &PullRequestListItem, query: &str) -> bool {
        let state_matches = match self.filters.state {
            StateFilter::Open => item.state == ChangeRequestState::Open,
            StateFilter::All => true,
            StateFilter::Closed => item.state == ChangeRequestState::Closed,
            StateFilter::Merged => item.state == ChangeRequestState::Merged,
        };
        let involvement_matches = match self.filters.involvement {
            InvolvementFilter::All | InvolvementFilter::Authored => true,
            InvolvementFilter::Reviewing => item.review == PullRequestReviewState::Required,
        };
        let draft_matches = match self.filters.draft {
            DraftFilter::All => true,
            DraftFilter::DraftsOnly => item.draft,
            DraftFilter::HideDrafts => !item.draft,
        };
        let review_matches = match self.filters.review {
            ReviewFilter::All => true,
            ReviewFilter::Approved => item.review == PullRequestReviewState::Approved,
            ReviewFilter::ChangesRequested => {
                item.review == PullRequestReviewState::ChangesRequested
            }
            ReviewFilter::Required => item.review == PullRequestReviewState::Required,
            ReviewFilter::None => item.review == PullRequestReviewState::None,
        };
        let checks_matches = match self.filters.checks {
            ChecksFilter::All => true,
            ChecksFilter::Passing => item.checks == PullRequestChecksState::Passing,
            ChecksFilter::Failing => item.checks == PullRequestChecksState::Failing,
            ChecksFilter::Pending => item.checks == PullRequestChecksState::Pending,
        };
        let host_matches = match self.filters.host {
            HostFilter::All => true,
            HostFilter::Github => item.provider == "github",
            HostFilter::Bitbucket => item.provider == "bitbucket",
        };
        let project_matches = match &self.filters.project {
            ProjectFilter::All => true,
            ProjectFilter::Repository(repository) => &item.repository == repository,
        };
        let query_matches = query.is_empty()
            || item.title.to_ascii_lowercase().contains(query)
            || item.repository.to_ascii_lowercase().contains(query)
            || item.provider.to_ascii_lowercase().contains(query)
            || item
                .author
                .as_deref()
                .is_some_and(|author| author.to_ascii_lowercase().contains(query));
        state_matches
            && involvement_matches
            && draft_matches
            && review_matches
            && checks_matches
            && host_matches
            && project_matches
            && query_matches
    }

    fn is_authored(&self, item: &PullRequestListItem) -> bool {
        self.filters.involvement == InvolvementFilter::Authored
            || item.author.as_deref().is_some_and(|author| {
                self.authored_accounts.iter().any(|(provider, account)| {
                    provider == &item.provider && author.eq_ignore_ascii_case(account)
                })
            })
    }

    fn section_header(theme: &Theme, label: &str, count: usize) -> gpui::Div {
        div()
            .mt(px(22.0))
            .mb(px(4.0))
            .pl(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted)
            .child(SharedString::from(label.to_string()))
            .child(
                div()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded_full()
                    .bg(theme.glass_hover())
                    .text_size(px(10.0))
                    .child(SharedString::from(count.to_string())),
            )
            .child(div().h(px(1.0)).flex_1().bg(theme.border.opacity(0.6)))
    }

    fn row(
        row: &PullRequestRow,
        theme: &Theme,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = row.item.clone();
        let selected_item = item.clone();
        let branch_color = match item.state {
            ChangeRequestState::Open => theme.success,
            ChangeRequestState::Merged => theme.accent,
            ChangeRequestState::Closed => theme.text_muted,
        };
        let (provider_icon, provider_color) = match item.provider.as_str() {
            "github" => (crate::icons::GITHUB_MARK, gpui::rgb(0xF0F0F0).into()),
            "bitbucket" => (crate::icons::BITBUCKET_MARK, gpui::rgb(0x2684FF).into()),
            _ => (crate::icons::GIT_BRANCH, branch_color),
        };
        let review = match item.review {
            PullRequestReviewState::Approved => Some(("Approved", theme.success)),
            PullRequestReviewState::ChangesRequested => Some(("Changes requested", theme.danger)),
            PullRequestReviewState::Required => Some(("Review required", theme.warning)),
            PullRequestReviewState::None => None,
        };
        let checks = match item.checks {
            PullRequestChecksState::Passing => Some(("Passing", theme.success)),
            PullRequestChecksState::Failing => Some(("Failing", theme.danger)),
            PullRequestChecksState::Pending => Some(("Pending", theme.warning)),
            PullRequestChecksState::None => None,
        };
        let author = item
            .author
            .as_deref()
            .map(|author| format!(" · {author}"))
            .unwrap_or_default();
        div()
            .id(SharedString::from(format!(
                "pull-request-{}-{}-{}",
                item.provider, item.repository, item.number
            )))
            .w_full()
            .px(px(10.0))
            .py(px(9.0))
            .rounded(px(8.0))
            .border_b_1()
            .border_color(theme.border.opacity(0.45))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .bg(if selected {
                theme.glass_hover()
            } else {
                gpui::transparent_black()
            })
            .hover(|s| s.bg(theme.glass_hover()))
            .on_click(cx.listener(move |page, _, _, cx| {
                page.selected = Some(selected_item.clone());
                cx.emit(PullRequestsEvent::SelectionChanged(page.selected.clone()));
                cx.notify();
            }))
            .child(
                crate::icons::icon(provider_icon)
                    .size(px(17.0))
                    .flex_none()
                    .text_color(provider_color),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(item.title)),
                    )
                    .child(
                        div()
                            .mt(px(3.0))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "#{} · {}{} · {} → {}",
                                item.number, item.repository, author, item.head_ref, item.base_ref
                            ))),
                    ),
            )
            .when(item.draft, |element| {
                Self::badge(element, "Draft", theme.text_muted)
            })
            .when_some(review, |element, (label, color)| {
                Self::badge(element, label, color)
            })
            .when_some(checks, |element, (label, color)| {
                Self::badge(element, label, color)
            })
            .into_any_element()
    }

    fn badge(
        element: gpui::Stateful<gpui::Div>,
        label: &str,
        color: gpui::Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        element.child(
            div()
                .flex_none()
                .px(px(6.0))
                .py(px(2.0))
                .rounded_full()
                .bg(color.opacity(0.12))
                .text_size(px(10.0))
                .text_color(color)
                .child(SharedString::from(label.to_string())),
        )
    }

    pub(crate) fn set_selection_silently(
        &mut self,
        item: Option<PullRequestListItem>,
        cx: &mut Context<Self>,
    ) {
        self.selected = item;
        cx.notify();
    }

    pub(crate) fn render_detail_surface_for(
        &mut self,
        item: &PullRequestListItem,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        Self::detail(item, &theme, window)
    }

    fn detail(item: &PullRequestListItem, theme: &Theme, window: &Window) -> AnyElement {
        let url = item.url.clone();
        let state = match item.state {
            ChangeRequestState::Open => ("Open", theme.success),
            ChangeRequestState::Merged => ("Merged", theme.accent),
            ChangeRequestState::Closed => ("Closed", theme.text_muted),
        };
        let review = match item.review {
            PullRequestReviewState::Approved => ("Approved", theme.success),
            PullRequestReviewState::ChangesRequested => ("Changes requested", theme.danger),
            PullRequestReviewState::Required => ("Review required", theme.warning),
            PullRequestReviewState::None => ("No review", theme.text_muted),
        };
        let checks = match item.checks {
            PullRequestChecksState::Passing => ("All checks passed", theme.success),
            PullRequestChecksState::Failing => ("Checks failing", theme.danger),
            PullRequestChecksState::Pending => ("Checks pending", theme.warning),
            PullRequestChecksState::None => ("Checks unavailable", theme.text_muted),
        };
        let provider = if item.provider == "github" {
            "GitHub"
        } else {
            "Bitbucket"
        };
        let (provider_icon, provider_color) = match item.provider.as_str() {
            "github" => (crate::icons::GITHUB_MARK, gpui::rgb(0xF0F0F0).into()),
            "bitbucket" => (crate::icons::BITBUCKET_MARK, gpui::rgb(0x2684FF).into()),
            _ => (crate::icons::PULL_REQUEST, theme.text_muted),
        };
        let description = item
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .unwrap_or("No description provided.");
        let description_tree = parse_full(description);
        let description_options = markdown_render::RenderOptions::settled(
            format!(
                "pull-request-description-{}-{}-{}",
                item.provider, item.repository, item.number
            )
            .into(),
        );
        div()
            .id("pull-request-detail")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .h(px(48.0))
                    .flex_none()
                    .px(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_1())
                    .child(
                        crate::settings::widgets::ghost_action(theme)
                            .id("pull-request-open-provider")
                            .hover(|s| crate::settings::widgets::ghost_hover(theme, s))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                            .child(
                                crate::icons::icon(crate::icons::ARROW_UP_RIGHT)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(format!("Open in {provider}"))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .px(px(20.0))
                    .py(px(20.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{}  #{}",
                                item.repository, item.number
                            ))),
                    )
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(item.title.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(
                                crate::icons::icon(provider_icon)
                                    .size(px(14.0))
                                    .text_color(provider_color),
                            )
                            .child(SharedString::from(format!(
                                "{}{}",
                                provider,
                                item.author
                                    .as_deref()
                                    .map(|author| format!(" · {author}"))
                                    .unwrap_or_default()
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(Self::badge(
                                div().id("pull-request-state-badge"),
                                state.0,
                                state.1,
                            ))
                            .child(
                                div()
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(6.0))
                                    .bg(theme.glass_hover())
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(format!(
                                        "{}  ←  {}",
                                        item.base_ref, item.head_ref
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .h(px(34.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(20.0))
                            .border_y_1()
                            .border_color(theme.border)
                            .text_size(px(12.0))
                            .child(Self::detail_status(theme, "Review", review.0, review.1))
                            .child(Self::detail_status(theme, "Checks", checks.0, checks.1))
                            .child(Self::detail_stat(
                                format!("{} files", item.changed_files),
                                theme.text_muted,
                            ))
                            .child(Self::detail_stat(
                                format!("+{} -{}", item.additions, item.deletions),
                                theme.text_muted,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(18.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Reviewers"))
                            .child(SharedString::from("No reviewers loaded"))
                            .child(SharedString::from("Comments  0")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from("Description")),
                            )
                            .child(markdown_render::render_tree(
                                &description_tree,
                                &description_options,
                                theme,
                                window,
                                &|_| None,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn detail_status(theme: &Theme, label: &str, value: &str, color: gpui::Hsla) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(SharedString::from(format!("{label}")))
            .child(div().size(px(7.0)).rounded_full().bg(color))
            .child(
                div()
                    .text_color(theme.text)
                    .child(SharedString::from(value.to_string())),
            )
    }

    fn detail_stat(value: String, color: gpui::Hsla) -> gpui::Div {
        div().text_color(color).child(SharedString::from(value))
    }

    fn filter_heading(theme: &Theme, label: &str) -> gpui::Div {
        div()
            .px(px(8.0))
            .pt(px(8.0))
            .pb(px(4.0))
            .text_size(px(10.0))
            .text_color(theme.text_muted.opacity(0.7))
            .child(SharedString::from(label.to_string()))
    }

    fn filter_option(
        theme: &Theme,
        id: impl Into<SharedString>,
        label: &str,
        active: bool,
        icon_path: &'static str,
    ) -> gpui::Stateful<gpui::Div> {
        let id = id.into();
        crate::popover::menu_row(theme, active, id.clone())
            .id(id)
            .child(
                crate::icons::icon(icon_path)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.8)),
            )
            .child(div().flex_1().child(SharedString::from(label.to_string())))
    }

    fn project_options(&self) -> Vec<String> {
        let mut projects: Vec<_> = match &self.pull_requests {
            Loadable::Ready(items) => items.iter().map(|item| item.repository.clone()).collect(),
            _ => Vec::new(),
        };
        projects.sort();
        projects.dedup();
        projects
    }

    fn render_filter_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let filters = self.filters.clone();
        let projects = self.project_options();
        let mut card = crate::popover::popover_card(theme)
            .id("pull-requests-filter-menu")
            .w(px(282.0))
            .max_h(px(620.0))
            .overflow_y_scroll()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_filter_menu(cx)));

        card = card
            .child(Self::filter_heading(theme, "State"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-state-all",
                    "All",
                    filters.state == StateFilter::All,
                    crate::icons::LIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_state(StateFilter::All, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-state-open",
                    "Open",
                    filters.state == StateFilter::Open,
                    crate::icons::GIT_BRANCH,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_state(StateFilter::Open, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-state-closed",
                    "Closed",
                    filters.state == StateFilter::Closed,
                    crate::icons::CLOSE_CIRCLE,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_state(StateFilter::Closed, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-state-merged",
                    "Merged",
                    filters.state == StateFilter::Merged,
                    crate::icons::CHECKLIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_state(StateFilter::Merged, cx))),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Involvement"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-involvement-all",
                    "All",
                    filters.involvement == InvolvementFilter::All,
                    crate::icons::LIST,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_involvement(InvolvementFilter::All, cx)),
                ),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-involvement-reviewing",
                    "Reviewing",
                    filters.involvement == InvolvementFilter::Reviewing,
                    crate::icons::PEN,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_involvement(InvolvementFilter::Reviewing, cx)
                })),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-involvement-authored",
                    "Authored",
                    filters.involvement == InvolvementFilter::Authored,
                    crate::icons::PEN_NEW_SQUARE,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_involvement(InvolvementFilter::Authored, cx)
                })),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Draft"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-draft-all",
                    "All",
                    filters.draft == DraftFilter::All,
                    crate::icons::LIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_draft(DraftFilter::All, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-draft-only",
                    "Drafts only",
                    filters.draft == DraftFilter::DraftsOnly,
                    crate::icons::PEN_NEW_SQUARE,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_draft(DraftFilter::DraftsOnly, cx)),
                ),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-draft-hide",
                    "Hide drafts",
                    filters.draft == DraftFilter::HideDrafts,
                    crate::icons::CLOSE_CIRCLE,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_draft(DraftFilter::HideDrafts, cx)),
                ),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Review"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-review-all",
                    "All",
                    filters.review == ReviewFilter::All,
                    crate::icons::LIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_review(ReviewFilter::All, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-review-approved",
                    "Approved",
                    filters.review == ReviewFilter::Approved,
                    crate::icons::CHECKLIST,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_review(ReviewFilter::Approved, cx)),
                ),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-review-changes",
                    "Changes requested",
                    filters.review == ReviewFilter::ChangesRequested,
                    crate::icons::PEN,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| {
                        this.set_review(ReviewFilter::ChangesRequested, cx)
                    }),
                ),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-review-required",
                    "Review required",
                    filters.review == ReviewFilter::Required,
                    crate::icons::INFO_CIRCLE,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.set_review(ReviewFilter::Required, cx)),
                ),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-review-none",
                    "No reviews",
                    filters.review == ReviewFilter::None,
                    crate::icons::CLOSE_CIRCLE,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_review(ReviewFilter::None, cx))),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Checks"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-checks-all",
                    "All",
                    filters.checks == ChecksFilter::All,
                    crate::icons::LIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_checks(ChecksFilter::All, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-checks-passing",
                    "Passing",
                    filters.checks == ChecksFilter::Passing,
                    crate::icons::CHECKLIST,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_checks(ChecksFilter::Passing, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-checks-failing",
                    "Failing",
                    filters.checks == ChecksFilter::Failing,
                    crate::icons::CLOSE_CIRCLE,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_checks(ChecksFilter::Failing, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-checks-pending",
                    "Pending",
                    filters.checks == ChecksFilter::Pending,
                    crate::icons::REFRESH,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_checks(ChecksFilter::Pending, cx))),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Host"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-host-all",
                    "All hosts",
                    filters.host == HostFilter::All,
                    crate::icons::GLOBAL,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_host(HostFilter::All, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-host-github",
                    "GitHub",
                    filters.host == HostFilter::Github,
                    crate::icons::GLOBAL,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_host(HostFilter::Github, cx))),
            )
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-host-bitbucket",
                    "Bitbucket",
                    filters.host == HostFilter::Bitbucket,
                    crate::icons::GLOBAL,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_host(HostFilter::Bitbucket, cx))),
            )
            .child(crate::popover::menu_separator())
            .child(Self::filter_heading(theme, "Project"))
            .child(
                Self::filter_option(
                    theme,
                    "pull-filter-project-all",
                    "All projects",
                    filters.project == ProjectFilter::All,
                    crate::icons::FOLDER,
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_project(ProjectFilter::All, cx))),
            );

        for (ix, project) in projects.into_iter().enumerate() {
            let selected = filters.project == ProjectFilter::Repository(project.clone());
            let value = ProjectFilter::Repository(project.clone());
            card = card.child(
                Self::filter_option(
                    theme,
                    format!("pull-filter-project-{ix}"),
                    &project,
                    selected,
                    crate::icons::FOLDER,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_project(value.clone(), cx))),
            );
        }
        card.into_any_element()
    }
}

impl Render for PullRequestsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let query = self.search.read(cx).text().trim().to_ascii_lowercase();
        let rows = match &self.pull_requests {
            Loadable::Ready(items) => self.rows(items, &query),
            _ => Vec::new(),
        };
        let review_requested: Vec<_> = rows
            .iter()
            .filter(|row| row.item.review == PullRequestReviewState::Required)
            .collect();
        let authored: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.item.review != PullRequestReviewState::Required && self.is_authored(&row.item)
            })
            .collect();
        let others: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.item.review != PullRequestReviewState::Required && !self.is_authored(&row.item)
            })
            .collect();
        let selected_key = self
            .selected
            .as_ref()
            .map(|item| (item.provider.clone(), item.repository.clone(), item.number));
        let is_selected = |item: &PullRequestListItem| {
            selected_key
                .as_ref()
                .is_some_and(|(provider, repository, number)| {
                    provider == &item.provider
                        && repository == &item.repository
                        && *number == item.number
                })
        };
        let list = match &self.pull_requests {
            Loadable::Idle | Loadable::Loading => div()
                .mt(px(24.0))
                .py(px(64.0))
                .flex()
                .flex_col()
                .items_center()
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .text_color(theme.text_muted)
                .child(crate::icons::icon(crate::icons::PULL_REQUEST).size(px(24.0)))
                .child(
                    div()
                        .mt(px(12.0))
                        .text_size(px(13.0))
                        .child(SharedString::from("Loading pull requests…")),
                )
                .into_any_element(),
            Loadable::Error(message) => div()
                .mt(px(24.0))
                .py(px(32.0))
                .px(px(20.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .text_color(theme.danger_muted)
                .child(SharedString::from(format!(
                    "Could not load pull requests: {message}"
                )))
                .into_any_element(),
            Loadable::Ready(_) if rows.is_empty() => div()
                .mt(px(24.0))
                .py(px(64.0))
                .flex()
                .flex_col()
                .items_center()
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .text_color(theme.text_muted)
                .child(crate::icons::icon(crate::icons::PULL_REQUEST).size(px(24.0)))
                .child(
                    div()
                        .mt(px(12.0))
                        .text_size(px(13.0))
                        .child(SharedString::from("No pull requests match these filters")),
                )
                .into_any_element(),
            Loadable::Ready(_) => div()
                .when(!review_requested.is_empty(), |element| {
                    element
                        .child(Self::section_header(
                            &theme,
                            "Review requested",
                            review_requested.len(),
                        ))
                        .children(
                            review_requested
                                .iter()
                                .map(|row| Self::row(row, &theme, is_selected(&row.item), cx)),
                        )
                })
                .when(!authored.is_empty(), |element| {
                    element
                        .child(Self::section_header(&theme, "Authored", authored.len()))
                        .children(
                            authored
                                .iter()
                                .map(|row| Self::row(row, &theme, is_selected(&row.item), cx)),
                        )
                })
                .when(!others.is_empty(), |element| {
                    element
                        .child(Self::section_header(&theme, "Others", others.len()))
                        .children(
                            others
                                .iter()
                                .map(|row| Self::row(row, &theme, is_selected(&row.item), cx)),
                        )
                })
                .into_any_element(),
        };
        let loading = matches!(self.pull_requests, Loadable::Loading);
        let filter_menu_open = self.filter_menu.get().is_some();
        let filter_menu_closing = self.filter_menu.closing_since();
        let filter_menu = filter_menu_open.then(|| {
            crate::popover::anchored_menu_below_gap(
                "pull-requests-filter-menu-layer",
                self.render_filter_menu(&theme, cx),
                filter_menu_closing,
                8.0,
            )
        });

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Pull Requests")),
            )
            .child(div().flex_1())
            .child(
                crate::settings::widgets::ghost_action(&theme)
                    .id("pull-requests-refresh")
                    .opacity(if loading { 0.45 } else { 1.0 })
                    .hover(|s| crate::settings::widgets::ghost_hover(&theme, s))
                    .on_click(cx.listener(|page, _, _, cx| page.load(cx, true)))
                    .child(
                        crate::icons::icon(crate::icons::REFRESH)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    ),
            );
        let toolbar = div()
            .mt(px(22.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .h(px(36.0))
                    .flex_1()
                    .min_w_0()
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.card_glass_bg())
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        crate::icons::icon(crate::icons::MAGNIFER)
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        div()
                            .id("pull-requests-filter")
                            .size(px(36.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(if filter_menu_open {
                                theme.accent.opacity(0.8)
                            } else {
                                theme.border
                            })
                            .bg(if filter_menu_open {
                                theme.glass_hover()
                            } else {
                                theme.card_glass_bg()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.glass_hover()))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, _| this.filter_menu.note_trigger_press()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_filter_menu(cx)))
                            .child(
                                crate::icons::icon(crate::icons::TUNING)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .when_some(filter_menu, |element, menu| element.child(menu)),
            );
        div()
            .id("pull-requests-page")
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .max_w(px(920.0))
                    .mx_auto()
                    .px(px(28.0))
                    .pt(px(24.0))
                    .pb(px(24.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(header)
                    .child(toolbar)
                    .child(
                        div()
                            .id("pull-requests-list-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(list),
                    ),
            )
    }
}
