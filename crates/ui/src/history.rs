//! Git history pane: paged commit rows plus a topological lane graph.
//!
//! The pane is hosted by the current Changes surface for now, but owns its
//! data and rendering so it can move intact into the future right-panel tabs.

use std::collections::HashSet;
use std::time::Duration;

use chrono::DateTime;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, EventEmitter, ListAlignment, ListState,
    PathBuilder, Render, SharedString, Subscription, Task, Window, canvas, container_query, div,
    list, point, prelude::*, px,
};
use zeron_proto::{GitHistoryCommit, GitHistoryPage, GitHistoryRef, GitHistoryRefKind};
use zeron_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

const HISTORY_PAGE_SIZE: usize = 100;
const HISTORY_ROW_HEIGHT: f32 = 36.0;
const HISTORY_LANE_SPACING: f32 = 12.0;
const HISTORY_NODE_RADIUS: f32 = 3.0;
const HISTORY_HEAD_RING_PADDING: f32 = 2.0;
const HISTORY_STROKE_WIDTH: f32 = 1.5;
const HISTORY_GRAPH_SATURATION: f32 = 0.72;
const HISTORY_GRAPH_SIDE_PADDING: f32 = 5.0;
const HISTORY_GRAPH_TRAILING_PADDING: f32 = 10.0;
const HISTORY_GRAPH_ROW_OVERLAP: f32 = 0.75;
const HISTORY_COMMIT_SUBJECT_MIN_WIDTH: f32 = 80.0;
const HISTORY_REF_AREA_RATIO: f32 = 0.45;
const HISTORY_REF_BADGE_MAX_WIDTH: f32 = 112.0;
const HISTORY_REF_GAP: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentShape {
    Through,
    Incoming,
    Outgoing,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphSegment {
    from_lane: usize,
    to_lane: usize,
    color_id: usize,
    shape: SegmentShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphRow {
    sha: String,
    node_lane: usize,
    node_color_id: usize,
    segments: Vec<GraphSegment>,
    is_head: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GraphLayout {
    rows: Vec<GraphRow>,
    max_lane_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveLane {
    id: usize,
    color_id: usize,
    target_sha: String,
}

fn lane_index(lanes: &[ActiveLane], id: usize) -> usize {
    lanes
        .iter()
        .position(|lane| lane.id == id)
        .expect("active Git history lane exists")
}

/// Commits arrive child-before-parent from `git log --topo-order`. Active
/// lanes point at the parent commit that will eventually resolve each path.
fn layout_graph(commits: &[GitHistoryCommit], head_sha: Option<&str>) -> GraphLayout {
    let mut active_lanes: Vec<ActiveLane> = Vec::new();
    let mut next_lane_id = 0usize;
    let mut next_color_id = 0usize;
    let mut max_lane_count = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let before = active_lanes.clone();
        let incoming: Vec<usize> = before
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| (lane.target_sha == commit.sha).then_some(index))
            .collect();
        let primary_incoming = incoming.first().copied();
        let node_lane = primary_incoming.unwrap_or(before.len());
        let primary_lane = primary_incoming.and_then(|index| before.get(index));
        let node_color_id = primary_lane.map(|lane| lane.color_id).unwrap_or_else(|| {
            let color = next_color_id;
            next_color_id += 1;
            color
        });
        let resolved_ids: HashSet<usize> = incoming.iter().map(|&index| before[index].id).collect();
        let mut next_lanes: Vec<ActiveLane> = before
            .iter()
            .filter(|lane| !resolved_ids.contains(&lane.id))
            .cloned()
            .collect();
        let mut outgoing: Vec<(usize, usize)> = Vec::new();

        let mut primary_outgoing_id = None;
        if let Some(first_parent) = commit.parent_shas.first() {
            let id = primary_lane.map(|lane| lane.id).unwrap_or_else(|| {
                let id = next_lane_id;
                next_lane_id += 1;
                id
            });
            let lane = ActiveLane {
                id,
                color_id: node_color_id,
                target_sha: first_parent.clone(),
            };
            next_lanes.insert(node_lane.min(next_lanes.len()), lane.clone());
            primary_outgoing_id = Some(id);
            outgoing.push((id, lane.color_id));
        }

        let mut parent_offset = 1usize;
        for parent_sha in commit.parent_shas.iter().skip(1) {
            if let Some(existing) = next_lanes
                .iter()
                .find(|lane| lane.target_sha == *parent_sha)
            {
                outgoing.push((existing.id, existing.color_id));
                continue;
            }
            let lane = ActiveLane {
                id: next_lane_id,
                color_id: next_color_id,
                target_sha: parent_sha.clone(),
            };
            next_lane_id += 1;
            next_color_id += 1;
            let primary_index = primary_outgoing_id
                .map(|id| lane_index(&next_lanes, id))
                .unwrap_or_else(|| node_lane.min(next_lanes.len()));
            next_lanes.insert(
                (primary_index + parent_offset).min(next_lanes.len()),
                lane.clone(),
            );
            parent_offset += 1;
            outgoing.push((lane.id, lane.color_id));
        }

        let mut segments: Vec<GraphSegment> = before
            .iter()
            .enumerate()
            .filter(|(_, lane)| !resolved_ids.contains(&lane.id))
            .map(|(from_lane, lane)| GraphSegment {
                from_lane,
                to_lane: lane_index(&next_lanes, lane.id),
                color_id: lane.color_id,
                shape: SegmentShape::Through,
            })
            .collect();
        segments.extend(incoming.iter().map(|&from_lane| GraphSegment {
            from_lane,
            to_lane: node_lane,
            color_id: before[from_lane].color_id,
            shape: SegmentShape::Incoming,
        }));
        segments.extend(outgoing.into_iter().map(|(id, color_id)| GraphSegment {
            from_lane: node_lane,
            to_lane: lane_index(&next_lanes, id),
            color_id,
            shape: SegmentShape::Outgoing,
        }));

        max_lane_count = max_lane_count
            .max(before.len())
            .max(next_lanes.len())
            .max(node_lane + 1);
        rows.push(GraphRow {
            sha: commit.sha.clone(),
            node_lane,
            node_color_id,
            segments,
            is_head: head_sha == Some(commit.sha.as_str()),
        });
        active_lanes = next_lanes;
    }

    GraphLayout {
        rows,
        max_lane_count,
    }
}

fn lane_x(lane: usize) -> f32 {
    HISTORY_GRAPH_SIDE_PADDING + HISTORY_NODE_RADIUS + lane as f32 * HISTORY_LANE_SPACING
}

fn graph_width(lane_count: usize) -> f32 {
    let count = lane_count.max(1);
    HISTORY_GRAPH_SIDE_PADDING
        + HISTORY_GRAPH_TRAILING_PADDING
        + HISTORY_NODE_RADIUS * 2.0
        + (count - 1) as f32 * HISTORY_LANE_SPACING
}

fn estimated_ref_badge_width(reference: &GitHistoryRef) -> f32 {
    // 10 px icon + 2 px gap + 10 px horizontal padding + the 10 px label.
    (22.0 + reference.label.chars().count() as f32 * 5.7).min(HISTORY_REF_BADGE_MAX_WIDTH)
}

fn estimated_ref_overflow_width(hidden: usize) -> f32 {
    format!("+{hidden}").chars().count() as f32 * 5.7
}

fn ref_area_width(commit_column_width: f32) -> f32 {
    let inner = (commit_column_width - 8.0).max(0.0);
    (inner * HISTORY_REF_AREA_RATIO)
        .min((inner - HISTORY_COMMIT_SUBJECT_MIN_WIDTH - HISTORY_REF_GAP).max(0.0))
}

fn visible_ref_count(refs: &[GitHistoryRef], available_width: f32) -> usize {
    if refs.is_empty() {
        return 0;
    }

    // Start with no visible badges so the overflow target remains available
    // when even the first badge plus `+N` would exceed the ref area.
    let mut visible = 0;
    for count in 1..=refs.len() {
        let hidden = refs.len() - count;
        let item_count = count + usize::from(hidden > 0);
        let badges = refs[..count]
            .iter()
            .map(estimated_ref_badge_width)
            .sum::<f32>();
        let overflow = (hidden > 0)
            .then(|| estimated_ref_overflow_width(hidden))
            .unwrap_or_default();
        let gaps = item_count.saturating_sub(1) as f32 * HISTORY_REF_GAP;
        if badges + overflow + gaps <= available_width {
            visible = count;
        }
    }
    visible
}

fn format_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|_| "—".to_string())
}

fn ref_color(reference: &GitHistoryRef, theme: &Theme) -> gpui::Hsla {
    match reference.kind {
        GitHistoryRefKind::Branch => theme.accent,
        GitHistoryRefKind::Remote => theme.busy,
        GitHistoryRefKind::Tag => theme.warning,
    }
}

fn ref_icon(kind: GitHistoryRefKind) -> &'static str {
    match kind {
        GitHistoryRefKind::Branch => crate::icons::GIT_BRANCH,
        GitHistoryRefKind::Remote => crate::icons::CLOUD,
        GitHistoryRefKind::Tag => crate::icons::TAG,
    }
}

fn ref_description(reference: &GitHistoryRef) -> SharedString {
    let kind = match reference.kind {
        GitHistoryRefKind::Branch => "Branch",
        GitHistoryRefKind::Remote => "Remote branch",
        GitHistoryRefKind::Tag => "Tag",
    };
    format!("{kind}: {}", reference.label).into()
}

fn graph_color(mut color: gpui::Hsla) -> gpui::Hsla {
    color.s *= HISTORY_GRAPH_SATURATION;
    color
}

struct HistoryRefTooltip {
    descriptions: Vec<SharedString>,
}

impl Render for HistoryRefTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let longest = self
            .descriptions
            .iter()
            .map(|description| description.chars().count())
            .max()
            .unwrap_or_default();
        let width = (longest as f32 * 6.4 + 16.0).clamp(72.0, 360.0);
        div()
            .w(px(width))
            .px(px(8.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .children(self.descriptions.iter().cloned().map(|description| {
                div()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .font_family(theme.font_mono.clone())
                    .child(description)
            }))
    }
}

pub struct GitHistory {
    state: Entity<AppState>,
    started: bool,
    target_key: Option<String>,
    commits: Vec<GitHistoryCommit>,
    head_sha: Option<String>,
    next_cursor: Option<usize>,
    total_count: Option<usize>,
    head_commit_count: Option<usize>,
    loading: bool,
    error: Option<SharedString>,
    graph: GraphLayout,
    list: ListState,
    copied_sha: Option<String>,
    request_task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    fetching_all: bool,
    fetch_for: Option<String>,
    fetch_error: Option<SharedString>,
    fetch_task: Option<Task<()>>,
    _observe: Subscription,
}

pub enum GitHistoryEvent {
    FetchSucceeded,
    /// A commit row was clicked — the host opens it as its own diff tab.
    OpenCommit(GitHistoryCommit),
}

impl EventEmitter<GitHistoryEvent> for GitHistory {}

pub struct GitHistoryCount {
    history: Entity<GitHistory>,
    _observe: Subscription,
}

pub struct GitHistoryFetchButton {
    history: Entity<GitHistory>,
    _observe: Subscription,
}

impl GitHistoryFetchButton {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl Render for GitHistoryFetchButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let fetching = self.history.read(cx).fetching_all;
        let history = self.history.clone();
        div()
            .id("history-fetch-all")
            .h(px(24.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .bg(if fetching {
                crate::theme::wash(0.05)
            } else {
                crate::motion::hover_blend(
                    "history-fetch-all",
                    crate::theme::wash(0.0),
                    crate::theme::wash(0.14),
                )
            })
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .when(!fetching, |element| {
                element
                    .cursor_pointer()
                    .on_hover(crate::motion::hover_listener("history-fetch-all"))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        history.update(cx, |history, cx| history.fetch_all(cx));
                    })
            })
            .child(if fetching {
                crate::loaders::mini_gradient_spinner(
                    "history-fetch-all-spinner",
                    1.75,
                    cx.entity_id(),
                    cx,
                )
                .into_any_element()
            } else {
                crate::icons::icon(crate::icons::CLOUD)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.75))
                    .into_any_element()
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(11.0))
                    .text_color(if fetching {
                        theme.text_faint
                    } else {
                        theme.text_muted
                    })
                    .child(if fetching { "Fetching…" } else { "Fetch all" }),
            )
    }
}

impl GitHistoryCount {
    pub fn new(history: Entity<GitHistory>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl Render for GitHistoryCount {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let history = self.history.read(cx);
        let count = history.commit_count();
        let branch = history.current_branch(cx);
        div()
            .h_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(10.0))
            .when_some(count, |element, count| {
                element.child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!(
                            "{count} commit{}",
                            if count == 1 { "" } else { "s" }
                        ))),
                )
            })
            .when_some(branch, |element, branch| {
                element.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.5))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(branch)),
                )
            })
    }
}

impl GitHistory {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this, _, cx| {
            if this.started {
                this.ensure_loaded(cx);
            }
        });
        Self {
            state,
            started: false,
            target_key: None,
            commits: Vec::new(),
            head_sha: None,
            next_cursor: None,
            total_count: None,
            head_commit_count: None,
            loading: false,
            error: None,
            graph: GraphLayout::default(),
            list: ListState::new(0, ListAlignment::Top, px(HISTORY_ROW_HEIGHT * 5.0)),
            copied_sha: None,
            request_task: None,
            copy_task: None,
            fetching_all: false,
            fetch_for: None,
            fetch_error: None,
            fetch_task: None,
            _observe: observe,
        }
    }

    fn context(&self, cx: &App) -> Option<(String, String, Option<String>)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let cwd = chat.cwd.clone()?;
        let target = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        let key = format!("{}|{cwd}", target.as_deref().unwrap_or("local"));
        Some((key, cwd, target))
    }

    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        self.started = true;
        let Some((key, cwd, target)) = self.context(cx) else {
            self.fetch_task = None;
            self.fetching_all = false;
            self.fetch_for = None;
            self.fetch_error = None;
            self.target_key = None;
            self.commits.clear();
            self.total_count = None;
            self.head_commit_count = None;
            self.graph = GraphLayout::default();
            self.list.reset(0);
            self.loading = false;
            return;
        };
        if self.target_key.as_deref() == Some(key.as_str()) {
            if !self.loading && self.commits.is_empty() && self.error.is_none() {
                self.fetch_page(key, cwd, target, 0, true, cx);
            }
            return;
        }
        self.request_task = None;
        self.fetch_task = None;
        self.fetching_all = false;
        self.fetch_for = None;
        self.fetch_error = None;
        self.loading = false;
        self.target_key = Some(key.clone());
        self.commits.clear();
        self.head_sha = None;
        self.next_cursor = None;
        self.total_count = None;
        self.head_commit_count = None;
        self.error = None;
        self.graph = GraphLayout::default();
        self.list.reset(0);
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.target_key = Some(key.clone());
        self.fetch_page(key, cwd, target, 0, true, cx);
    }

    pub fn fetch_all(&mut self, cx: &mut Context<Self>) {
        if self.fetching_all {
            return;
        }
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.fetching_all = true;
        self.fetch_for = Some(key.clone());
        self.fetch_error = None;
        cx.notify();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd.clone()));
            if let Some(target) = target.clone() {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::FETCH_ALL, serde_json::Value::Object(params))
                .await;
            this.update(cx, |history, cx| {
                if history.fetch_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                history.fetching_all = false;
                history.fetch_for = None;
                match result {
                    Ok(_) => {
                        history.fetch_error = None;
                        // Cancel a pre-fetch history request so the next page
                        // is guaranteed to observe the updated remote refs.
                        history.request_task = None;
                        history.loading = false;
                        history.fetch_page(key, cwd, target, 0, true, cx);
                        cx.emit(GitHistoryEvent::FetchSucceeded);
                    }
                    Err(error) => history.fetch_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn commit_count(&self) -> Option<usize> {
        self.head_commit_count
    }

    fn current_branch(&self, cx: &App) -> Option<String> {
        self.state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.branch.clone())
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(cursor) = self.next_cursor else {
            return;
        };
        let Some((key, cwd, target)) = self.context(cx) else {
            return;
        };
        self.fetch_page(key, cwd, target, cursor, false, cx);
    }

    fn fetch_page(
        &mut self,
        key: String,
        cwd: String,
        target: Option<String>,
        cursor: usize,
        reset: bool,
        cx: &mut Context<Self>,
    ) {
        if self.loading {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("cursor".into(), serde_json::json!(cursor));
            params.insert("limit".into(), serde_json::json!(HISTORY_PAGE_SIZE));
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::LIST_GIT_HISTORY, serde_json::Value::Object(params))
                .await;
            this.update(cx, |history, cx| {
                if history.target_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                history.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<GitHistoryPage>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(page) => {
                        let old_commit_count = history.commits.len();
                        let old_item_count =
                            old_commit_count + usize::from(history.next_cursor.is_some());
                        if reset {
                            history.commits = page.commits;
                            history.total_count = page.total_count;
                            history.head_commit_count = page.head_commit_count;
                        } else {
                            let mut seen: HashSet<String> = history
                                .commits
                                .iter()
                                .map(|commit| commit.sha.clone())
                                .collect();
                            history.commits.extend(
                                page.commits
                                    .into_iter()
                                    .filter(|commit| seen.insert(commit.sha.clone())),
                            );
                            if page.total_count.is_some() {
                                history.total_count = page.total_count;
                            }
                            if page.head_commit_count.is_some() {
                                history.head_commit_count = page.head_commit_count;
                            }
                        }
                        history.head_sha = page.head_sha;
                        history.next_cursor = page.next_cursor;
                        history.graph = layout_graph(&history.commits, history.head_sha.as_deref());
                        let new_item_count =
                            history.commits.len() + usize::from(history.next_cursor.is_some());
                        if reset {
                            history
                                .list
                                .reset_with_uniform_height(new_item_count, px(HISTORY_ROW_HEIGHT));
                        } else {
                            history.list.splice(
                                old_commit_count..old_item_count,
                                new_item_count - old_commit_count,
                            );
                        }
                    }
                    Err(error) => history.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_sha(&mut self, sha: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
        self.copied_sha = Some(sha);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1_200))
                .await;
            this.update(cx, |history, cx| {
                history.copied_sha = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn graph_paths(&self, theme: &Theme) -> AnyElement {
        let palette = [
            graph_color(theme.accent),
            graph_color(theme.busy),
            graph_color(theme.success),
            graph_color(theme.warning),
            graph_color(theme.danger),
            graph_color(theme.text_muted),
        ];
        let rows = self.graph.rows.clone();
        let list = self.list.clone();
        canvas(
            |_, _, _| (),
            move |viewport_bounds, _, window, _| {
                let middle = HISTORY_ROW_HEIGHT / 2.0;
                for (color_index, color) in palette.iter().enumerate() {
                    let mut builder = PathBuilder::stroke(px(HISTORY_STROKE_WIDTH));
                    let mut has_segments = false;

                    for (index, row) in rows.iter().enumerate() {
                        let Some(row_bounds) = list.bounds_for_item(index) else {
                            continue;
                        };
                        if row_bounds.bottom() < viewport_bounds.top()
                            || row_bounds.top() > viewport_bounds.bottom()
                        {
                            continue;
                        }
                        for segment in row
                            .segments
                            .iter()
                            .filter(|segment| segment.color_id % palette.len() == color_index)
                        {
                            has_segments = true;
                            let from_x = lane_x(segment.from_lane);
                            let to_x = lane_x(segment.to_lane);
                            let origin = row_bounds.origin;
                            match segment.shape {
                                SegmentShape::Incoming => {
                                    builder.move_to(point(
                                        origin.x + px(from_x),
                                        origin.y - px(HISTORY_GRAPH_ROW_OVERLAP),
                                    ));
                                    builder.cubic_bezier_to(
                                        point(origin.x + px(to_x), origin.y + px(middle)),
                                        point(origin.x + px(from_x), origin.y + px(middle * 0.55)),
                                        point(origin.x + px(to_x), origin.y + px(middle * 0.55)),
                                    );
                                }
                                SegmentShape::Outgoing => {
                                    builder.move_to(point(
                                        origin.x + px(from_x),
                                        origin.y + px(middle),
                                    ));
                                    builder.cubic_bezier_to(
                                        point(
                                            origin.x + px(to_x),
                                            origin.y
                                                + px(
                                                    HISTORY_ROW_HEIGHT
                                                        + HISTORY_GRAPH_ROW_OVERLAP,
                                                ),
                                        ),
                                        point(
                                            origin.x + px(from_x),
                                            origin.y + px(middle * 1.45),
                                        ),
                                        point(
                                            origin.x + px(to_x),
                                            origin.y + px(middle * 1.45),
                                        ),
                                    );
                                }
                                SegmentShape::Through => {
                                    builder.move_to(point(
                                        origin.x + px(from_x),
                                        origin.y - px(HISTORY_GRAPH_ROW_OVERLAP),
                                    ));
                                    let end = point(
                                        origin.x + px(to_x),
                                        origin.y
                                            + px(HISTORY_ROW_HEIGHT + HISTORY_GRAPH_ROW_OVERLAP),
                                    );
                                    if segment.from_lane == segment.to_lane {
                                        builder.line_to(end);
                                    } else {
                                        builder.cubic_bezier_to(
                                            end,
                                            point(origin.x + px(from_x), origin.y + px(middle)),
                                            point(origin.x + px(to_x), origin.y + px(middle)),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    if has_segments && let Ok(path) = builder.build() {
                        window.paint_path(path, *color);
                    }
                }
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    fn graph_cell(row: GraphRow, lane_count: usize, theme: &Theme) -> AnyElement {
        let width = graph_width(lane_count);
        let palette = [
            graph_color(theme.accent),
            graph_color(theme.busy),
            graph_color(theme.success),
            graph_color(theme.warning),
            graph_color(theme.danger),
            graph_color(theme.text_muted),
        ];
        let color = palette[row.node_color_id % palette.len()];
        let node_x = lane_x(row.node_lane);
        let node = div()
            .absolute()
            .left(px(node_x - HISTORY_NODE_RADIUS))
            .top(px(HISTORY_ROW_HEIGHT / 2.0 - HISTORY_NODE_RADIUS))
            .size(px(HISTORY_NODE_RADIUS * 2.0))
            .rounded_full()
            .bg(color);
        div()
            .relative()
            .w(px(width))
            .h(px(HISTORY_ROW_HEIGHT))
            .flex_none()
            .when(row.is_head, |element| {
                element.child(
                    div()
                        .absolute()
                        .left(px(node_x - HISTORY_NODE_RADIUS - HISTORY_HEAD_RING_PADDING))
                        .top(px(HISTORY_ROW_HEIGHT / 2.0
                            - HISTORY_NODE_RADIUS
                            - HISTORY_HEAD_RING_PADDING))
                        .size(px((HISTORY_NODE_RADIUS + HISTORY_HEAD_RING_PADDING) * 2.0))
                        .rounded_full()
                        .border_1()
                        .border_color(color)
                        .bg(theme.bg),
                )
            })
            .child(node)
            .into_any_element()
    }

    fn render_ref(
        reference: GitHistoryRef,
        row_index: usize,
        ref_index: usize,
        theme: &Theme,
    ) -> AnyElement {
        let color = ref_color(&reference, theme);
        let icon = ref_icon(reference.kind);
        let description = ref_description(&reference);
        div()
            .id(SharedString::from(format!(
                "history-ref-{row_index}-{ref_index}"
            )))
            .h(px(16.0))
            .max_w(px(112.0))
            .px(px(5.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .rounded(px(4.0))
            .bg(color.opacity(0.07))
            .text_size(px(10.0))
            .text_color(color.opacity(0.9))
            .child(
                crate::icons::icon(icon)
                    .size(px(10.0))
                    .mt(px(1.0))
                    .text_color(color.opacity(0.78)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(reference.label)),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| HistoryRefTooltip {
                    descriptions: vec![description.clone()],
                })
                .into()
            })
            .tooltip_show_delay(Duration::from_millis(350))
            .into_any_element()
    }

    fn render_ref_area(
        refs: Vec<GitHistoryRef>,
        row_index: usize,
        available_width: f32,
        theme: &Theme,
    ) -> AnyElement {
        let visible_count = visible_ref_count(&refs, available_width);
        let hidden_refs: Vec<_> = refs.iter().skip(visible_count).cloned().collect();
        let hidden_count = hidden_refs.len();
        let hidden_descriptions: Vec<_> = hidden_refs.iter().map(ref_description).collect();

        div()
            .max_w(px(available_width))
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(HISTORY_REF_GAP))
            .children(refs.into_iter().take(visible_count).enumerate().map(
                |(ref_index, reference)| Self::render_ref(reference, row_index, ref_index, theme),
            ))
            .when(hidden_count > 0, |element| {
                element.child(
                    div()
                        .id(("history-ref-overflow", row_index))
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("+{hidden_count}")))
                        .tooltip(move |_, cx| {
                            cx.new(|_| HistoryRefTooltip {
                                descriptions: hidden_descriptions.clone(),
                            })
                            .into()
                        })
                        .tooltip_show_delay(Duration::from_millis(350)),
                )
            })
            .into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if index >= self.commits.len() {
            let theme = Theme::of(cx).clone();
            let has_error = self.error.is_some();
            let label = if self.loading {
                "Loading…"
            } else if has_error {
                "Retry"
            } else {
                "Load more"
            };
            let button = div()
                .id("history-load-older")
                .h(px(28.0))
                .px(px(11.0))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border.opacity(0.85))
                .bg(theme.surface_raised.opacity(0.72))
                .text_size(px(11.0))
                .text_color(if self.loading {
                    theme.text_faint
                } else {
                    theme.text_muted
                })
                .when(!self.loading, |element| {
                    element
                        .cursor_pointer()
                        .hover(|style| {
                            style
                                .bg(theme.element_hover)
                                .border_color(theme.border_strong.opacity(0.75))
                                .text_color(theme.text)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.load_older(cx)))
                })
                .when(!self.loading, |element| {
                    element.child(
                        crate::icons::icon(if has_error {
                            crate::icons::REFRESH
                        } else {
                            crate::icons::ALT_ARROW_DOWN
                        })
                        .size(px(11.0))
                        .flex_none()
                        .text_color(theme.text_faint),
                    )
                })
                .child(SharedString::from(label));
            return div()
                .w_full()
                .h(px(48.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(button)
                .into_any_element();
        }
        let Some(commit) = self.commits.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let Some(graph_row) = self.graph.rows.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let sha = commit.sha.clone();
        let open_commit = commit.clone();
        let copied = self.copied_sha.as_deref() == Some(sha.as_str());
        let commit_subject = if commit.subject.is_empty() {
            "(no subject)".to_string()
        } else {
            commit.subject
        };
        let commit_refs = commit.refs;
        let commit_theme = theme.clone();

        div()
            .id(("history-row", index))
            .h(px(HISTORY_ROW_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .border_b_1()
            .border_color(crate::theme::hairline(0.04))
            .text_size(px(11.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.025)))
            // A commit row click opens the commit as its own diff tab (the
            // host — the right pane's surface strip — listens; user request).
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(GitHistoryEvent::OpenCommit(open_commit.clone()));
            }))
            .child(Self::graph_cell(
                graph_row,
                self.graph.max_lane_count,
                &theme,
            ))
            .child(
                container_query(move |size, _, _| {
                    let refs_width = ref_area_width(f32::from(size.width));
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .gap(px(HISTORY_REF_GAP))
                        .pr(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(commit_theme.text)
                                .child(SharedString::from(commit_subject.clone())),
                        )
                        .when(!commit_refs.is_empty(), |element| {
                            element.child(Self::render_ref_area(
                                commit_refs.clone(),
                                index,
                                refs_width,
                                &commit_theme,
                            ))
                        })
                })
                .flex_1()
                .min_w(px(HISTORY_COMMIT_SUBJECT_MIN_WIDTH))
                .overflow_hidden(),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .truncate()
                    .pr(px(8.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(if commit.author_name.is_empty() {
                        "Unknown".to_string()
                    } else {
                        commit.author_name
                    })),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .truncate()
                    .pr(px(8.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format_date(&commit.authored_at))),
            )
            .child(
                div()
                    .id(SharedString::from(format!("history-sha-{index}")))
                    .w(px(68.0))
                    .h(px(24.0))
                    .mr(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.07)))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(if copied {
                        theme.accent
                    } else {
                        theme.text_muted
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // The sha chip must not ALSO open the commit tab.
                        cx.stop_propagation();
                        this.copy_sha(sha.clone(), cx)
                    }))
                    .child(SharedString::from(if copied {
                        "Copied".to_string()
                    } else {
                        commit.sha.chars().take(7).collect()
                    })),
            )
            .into_any_element()
    }
}

impl Render for GitHistory {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let theme = Theme::of(cx).clone();
        let graph_column = graph_width(self.graph.max_lane_count);

        let body: AnyElement = if self.target_key.is_none() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("No repository selected")
                .into_any_element()
        } else if self.loading && self.commits.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(crate::loaders::gradient_spinner(
                    "history-loading",
                    &theme,
                    3.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading history…"),
                )
                .into_any_element()
        } else if self.commits.is_empty() {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| SharedString::from("No commits found"));
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.0))
                .text_size(px(12.0))
                .text_color(if self.error.is_some() {
                    theme.warning
                } else {
                    theme.text_faint
                })
                .child(message)
                .into_any_element()
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(self.graph_paths(&theme))
                .child(
                    list(self.list.clone(), cx.processor(Self::render_row))
                        .size_full()
                        .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(self.fetch_error.clone(), |element, error| {
                element.child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .border_b_1()
                        .border_color(theme.danger.opacity(0.16))
                        .bg(theme.danger.opacity(0.05))
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger_muted)
                        .child(SharedString::from(format!("Fetch failed: {error}"))),
                )
            })
            .when_some(
                self.error.clone().filter(|_| !self.commits.is_empty()),
                |element, error| {
                    element.child(
                        div()
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(8.0))
                            .border_b_1()
                            .border_color(theme.danger.opacity(0.16))
                            .bg(theme.danger.opacity(0.05))
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme.danger_muted)
                            .child(error),
                    )
                },
            )
            .when(!self.commits.is_empty(), |element| {
                element.child(
                    div()
                        .h(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(crate::theme::hairline(0.06))
                        .text_size(px(9.5))
                        .text_color(theme.text_faint)
                        .child(div().w(px(graph_column)).flex_none())
                        .child(div().flex_1().min_w(px(80.0)).child("Commit"))
                        .child(div().w(px(88.0)).flex_none().child("Author"))
                        .child(div().w(px(88.0)).flex_none().child("Date"))
                        .child(div().w(px(74.0)).flex_none().child("SHA")),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> GitHistoryCommit {
        GitHistoryCommit {
            sha: sha.into(),
            parent_shas: parents.iter().map(|value| (*value).into()).collect(),
            subject: sha.into(),
            author_name: "Test".into(),
            author_email: "test@example.com".into(),
            authored_at: "2026-08-12T12:00:00Z".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn graph_splits_and_rejoins_merge_lanes() {
        let commits = vec![
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        let graph = layout_graph(&commits, Some("merge"));
        assert_eq!(graph.rows.len(), commits.len());
        assert!(graph.max_lane_count >= 2);
        assert!(graph.rows[0].is_head);
        assert_eq!(graph.rows[0].segments.len(), 2);
        assert_eq!(graph.rows[3].segments.len(), 2);
        assert_eq!(graph.rows[3].node_lane, 0);
    }

    #[test]
    fn appending_older_commits_preserves_the_loaded_prefix_layout() {
        let prefix = vec![commit("tip", &["parent"]), commit("parent", &["root"])];
        let before = layout_graph(&prefix, Some("tip"));
        let mut all = prefix.clone();
        all.push(commit("root", &[]));
        let after = layout_graph(&all, Some("tip"));
        assert_eq!(before.rows, after.rows[..before.rows.len()]);
    }

    #[test]
    fn graph_palette_only_reduces_saturation() {
        let source = gpui::hsla(0.62, 0.8, 0.55, 0.9);
        let muted = graph_color(source);
        assert_eq!(muted.h, source.h);
        assert_eq!(muted.l, source.l);
        assert_eq!(muted.a, source.a);
        assert!((muted.s - source.s * HISTORY_GRAPH_SATURATION).abs() < f32::EPSILON);
    }

    #[test]
    fn ref_badges_expand_with_the_available_width() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![reference("main"), reference("tag"), reference("origin")];
        assert_eq!(visible_ref_count(&[], 100.0), 0);
        assert_eq!(visible_ref_count(&refs, 70.0), 1);
        assert_eq!(visible_ref_count(&refs, 115.0), 2);
        assert_eq!(visible_ref_count(&refs, 200.0), 3);
    }

    #[test]
    fn ref_badges_preserve_overflow_when_the_first_badge_is_too_wide() {
        let reference = |label: &str| GitHistoryRef {
            kind: GitHistoryRefKind::Branch,
            label: label.into(),
        };
        let refs = vec![
            reference("feature/accessibility-polish"),
            reference("main"),
            reference("origin/main"),
            reference("upstream/main"),
            reference("v0.1.53"),
            reference("HEAD"),
        ];

        assert_eq!(visible_ref_count(&refs, 120.0), 0);
    }

    #[test]
    fn ref_area_preserves_subject_space_and_caps_at_forty_five_percent() {
        assert_eq!(ref_area_width(80.0), 0.0);
        assert!((ref_area_width(200.0) - 86.4).abs() < 0.001);
        assert!((ref_area_width(400.0) - 176.4).abs() < 0.001);
    }

    #[test]
    fn ref_tooltip_describes_each_reference_kind() {
        let reference = |kind: GitHistoryRefKind, label: &str| GitHistoryRef {
            kind,
            label: label.into(),
        };
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Branch, "main")),
            "Branch: main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Remote, "origin/main")),
            "Remote branch: origin/main"
        );
        assert_eq!(
            ref_description(&reference(GitHistoryRefKind::Tag, "v0.1.52")),
            "Tag: v0.1.52"
        );
    }
}
