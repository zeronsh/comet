//! Shared diff presentation for the Changes pane and transcript tool details.
//! No checkout, RPC, history, or review-comment orchestration lives here.

use std::sync::Arc;

use gpui::{AnyElement, SharedString, div, font, prelude::*, px};

use crate::markdown::render;
use crate::theme::Theme;

pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 21.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const BODY_BOTTOM_PAD: f32 = 8.0;
/// Gutter width per line-number column.
pub const GUTTER_WIDTH: f32 = 36.0;
/// The +/−/· marker column between the gutters and the code.
pub const MARKER_WIDTH: f32 = 28.0;
/// Width of the coloured accent bar on the left edge of +/− rows.
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
pub(super) const DIFF_TEXT_SIZE: f32 = 12.0;
pub(super) const UNIFIED_CODE_PADDING_LEFT: f32 = 12.0;
pub(super) const SPLIT_CODE_PADDING_LEFT: f32 = 6.0;
/// Breathing room after the widest source line when scrolled fully right.
pub(super) const CODE_PADDING_RIGHT: f32 = 24.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    /// `\ No newline at end of file` and friends.
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceLineRef {
    pub side: SourceSide,
    /// One-based source line number.
    pub line_number: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHighlights {
    pub old: Option<Arc<zeron_syntax::HighlightedDocument>>,
    pub new: Option<Arc<zeron_syntax::HighlightedDocument>>,
}

impl DiffHighlights {
    pub fn source_ref(&self, line: &DiffLine) -> Option<SourceLineRef> {
        match line.kind {
            LineKind::Del => line.old_no.map(|line_number| SourceLineRef {
                side: SourceSide::Old,
                line_number,
            }),
            LineKind::Add => line.new_no.map(|line_number| SourceLineRef {
                side: SourceSide::New,
                line_number,
            }),
            LineKind::Context => line
                .new_no
                .filter(|_| self.new.is_some())
                .map(|line_number| SourceLineRef {
                    side: SourceSide::New,
                    line_number,
                })
                .or_else(|| {
                    line.old_no.map(|line_number| SourceLineRef {
                        side: SourceSide::Old,
                        line_number,
                    })
                }),
            LineKind::Meta => None,
        }
    }

    pub fn spans(&self, line: &DiffLine) -> &[zeron_syntax::HighlightSpan] {
        let Some(source_ref) = self.source_ref(line) else {
            return &[];
        };
        let document = match source_ref.side {
            SourceSide::Old => self.old.as_deref(),
            SourceSide::New => self.new.as_deref(),
        };
        document
            .and_then(|document| document.lines.get(source_ref.line_number as usize - 1))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// Display path (the post-change side).
    pub path: String,
    /// Pre-rename path, when different.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Parser-collected notices (mode changes etc.).
    pub notices: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
    /// Largest line number on either side — sizes the gutters analytically
    /// (a fixed column overflowed past 4 digits; user report).
    pub max_line: u32,
}

impl FileDiff {
    pub(super) fn new(path: String, old_path: Option<String>) -> Self {
        Self {
            path,
            old_path,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
            max_line: 0,
        }
    }
}

/// Width of one line-number gutter column, fitted to the file's largest
/// line number: 11px mono ≈ 6.6px per digit, the 8px right pad, and a 6px
/// left gap so the number never abuts the accent bar (at 4 digits the old
/// formula left 1.6px — visually touching; user report). Never narrower
/// than the classic 36px column.
pub fn gutter_width(file: &FileDiff) -> f32 {
    let digits = file.max_line.max(1).ilog10() + 1;
    (digits as f32 * 6.6 + 8.0 + 6.0).max(GUTTER_WIDTH)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DiffHorizontalMetrics {
    pub(super) max_text_width: f32,
    pub(super) max_gutter_width: f32,
}

impl DiffHorizontalMetrics {
    /// Compensating for the file-local gutter keeps every unified code
    /// viewport's effective scroll range identical.
    pub(super) fn unified_content_width(self, gutter_width: f32) -> f32 {
        self.max_text_width
            + UNIFIED_CODE_PADDING_LEFT
            + CODE_PADDING_RIGHT
            + 2.0 * (self.max_gutter_width - gutter_width)
    }

    /// Split has one gutter per half. Both halves use this same extent so old
    /// and new remain synchronized even when one side is a filler.
    pub(super) fn split_content_width(self, gutter_width: f32) -> f32 {
        self.max_text_width
            + SPLIT_CODE_PADDING_LEFT
            + CODE_PADDING_RIGHT
            + (self.max_gutter_width - gutter_width)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DiffCodeWidth {
    /// Inline tool diffs keep their existing local clipping behavior.
    Clipped,
    /// Changes rows expose a stable intrinsic code width.
    Scrollable(DiffHorizontalMetrics),
    /// Changes rows consume their viewport width and grow vertically.
    Wrapped,
}

#[derive(Clone)]
pub(super) struct DiffCodeScroll {
    pub(super) handle: gpui::ScrollHandle,
    pub(super) id: SharedString,
}

/// Derived per-file notice rows (new/deleted/renamed/binary + parser notices).
pub fn file_notices(file: &FileDiff) -> Vec<String> {
    let mut notices = Vec::new();
    match file.status {
        FileStatus::Added => notices.push("New file".to_string()),
        FileStatus::Deleted => notices.push("Deleted file".to_string()),
        FileStatus::Renamed => {
            let from = file.old_path.as_deref().unwrap_or("?");
            notices.push(format!("Renamed from {from}"));
        }
        FileStatus::Modified => {}
    }
    if file.binary {
        notices.push("Binary file — contents not shown".to_string());
    }
    notices.extend(file.notices.iter().cloned());
    notices
}

/// Cap a file's hunks at `max_lines` total diff lines, appending a notice
/// when lines were dropped. The transcript renders a tool diff as ONE
/// stacked element inside its row, so an unbounded diff (a fetched
/// full-diff blob, a whole-file rewrite) would otherwise build tens of
/// thousands of elements every frame it is visible.
pub fn truncate_file_lines(file: &mut FileDiff, max_lines: usize) {
    let total: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    if total <= max_lines {
        return;
    }
    let mut budget = max_lines;
    file.hunks.retain_mut(|hunk| {
        if budget == 0 {
            return false;
        }
        if hunk.lines.len() > budget {
            hunk.lines.truncate(budget);
        }
        budget -= hunk.lines.len();
        true
    });
    file.notices.push(format!(
        "Diff truncated — showing first {max_lines} of {total} lines"
    ));
    // The gutter fits what actually renders.
    file.max_line = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.old_no.unwrap_or(0).max(l.new_no.unwrap_or(0)))
        .max()
        .unwrap_or(0);
}

/// Analytic expanded-body height — drives the 180 ms fold tween without
/// measurement.
pub fn body_height(file: &FileDiff) -> f32 {
    // The same row order as the unified body, without constructing the native
    // pane's comment-aware row model.
    file_notices(file)
        .iter()
        .map(|_| NOTICE_HEIGHT)
        .chain(file.hunks.iter().flat_map(|hunk| {
            std::iter::once(HUNK_HEADER_HEIGHT).chain(hunk.lines.iter().map(|_| DIFF_LINE_HEIGHT))
        }))
        .chain(std::iter::once(BODY_BOTTOM_PAD))
        .sum()
}

/// Green for additions — sampled from the reference diff (soft emerald).
pub(super) fn add_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_add // emerald-400
}

/// Red for deletions — softer than the theme danger, per the reference diff.
pub(super) fn del_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_del // red-400
}

/// One notice row ("New file", "Binary file — contents not shown", …).
pub(super) fn notice_row(notice: String, theme: &Theme) -> AnyElement {
    div()
        .h(px(NOTICE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(notice))
        .into_any_element()
}

/// One `@@ … @@` hunk-header row on the bluish-grey wash.
pub(super) fn hunk_header_row(header: &str, theme: &Theme) -> AnyElement {
    div()
        .h(px(HUNK_HEADER_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .bg(theme.diff_hunk_bg)
        .font_family(theme.font_mono.clone())
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(header.to_string()))
        .into_any_element()
}

/// The only part of a diff row allowed to exceed its viewport. The outer
/// element keeps row chrome fixed; the inner element owns the intrinsic code
/// width and is the only plane moved by the file's horizontal scroll handle.
pub(super) fn code_text_viewport(
    text: String,
    runs: Vec<gpui::TextRun>,
    theme: &Theme,
    padding_left: f32,
    content_width: Option<f32>,
    wrapped: bool,
    scroll: Option<DiffCodeScroll>,
) -> AnyElement {
    let content = div()
        .when(wrapped, |el| el.w_full().min_w_0())
        .when_some(content_width, |el, width| {
            // Keep every tracked row's scroll extent identical. The width
            // already includes shaping slack on the right, so clipping here
            // only prevents a child from redefining the shared maximum.
            el.w(px(width)).flex_none().overflow_hidden()
        })
        .pl(px(padding_left))
        .font_family(theme.font_mono.clone())
        .text_size(px(DIFF_TEXT_SIZE))
        .line_height(px(DIFF_LINE_HEIGHT))
        .map(|el| {
            if wrapped {
                el.whitespace_normal()
            } else {
                el.whitespace_nowrap()
            }
        })
        .child(gpui::StyledText::new(text).with_runs(runs));
    let viewport = div()
        .flex_1()
        .min_w_0()
        .min_h(px(DIFF_LINE_HEIGHT))
        .overflow_hidden()
        .child(content);
    if wrapped {
        return viewport.into_any_element();
    }
    match scroll {
        Some(scroll) => {
            let mut viewport = viewport
                .id(scroll.id)
                .overflow_x_scroll()
                .track_scroll(&scroll.handle);
            // Without this GPUI maps a vertical-only wheel delta onto x for
            // an x-only scroller, starving the virtualized list underneath.
            viewport.style().restrict_scroll_to_axis = Some(true);
            viewport.into_any_element()
        }
        None => viewport.into_any_element(),
    }
}

/// One +/−/context/meta diff line: coloured accent bar, dual line-number
/// gutters (`gutter_px` wide — see [`gutter_width`]), marker column, and
/// paint-only syntax runs.
pub(super) fn diff_line_row(
    line: &DiffLine,
    spans: &[zeron_syntax::HighlightSpan],
    theme: &Theme,
    gutter_px: f32,
    code_width: DiffCodeWidth,
    scroll: Option<DiffCodeScroll>,
) -> AnyElement {
    if line.kind == LineKind::Meta {
        return meta_line_row(
            &line.text,
            theme,
            ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0,
        );
    }

    // Row tints sampled from the reference: ~5–6% washes over the pane tone.
    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;

    let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
        LineKind::Add => (
            "+",
            add_color(theme),
            Some(add_bg),
            Some(add_color(theme).opacity(0.55)),
            add_color(theme).opacity(0.9),
        ),
        LineKind::Del => (
            "−",
            del_color(theme),
            Some(del_bg),
            Some(del_color(theme).opacity(0.55)),
            del_color(theme).opacity(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.opacity(0.5),
            None,
            None,
            theme.text_faint.opacity(0.8),
        ),
    };
    let gutter = |no: Option<u32>, color: gpui::Hsla| {
        div()
            .w(px(gutter_px))
            .flex_none()
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .line_height(px(DIFF_LINE_HEIGHT))
            .text_color(color)
            .flex()
            .justify_end()
            .pr(px(8.0))
            .child(SharedString::from(
                no.map(|n| n.to_string()).unwrap_or_default(),
            ))
    };
    let mono = font(theme.font_mono.clone());
    let runs = render::runs_for_syntax_line_with_plain(
        &line.text,
        spans,
        &mono,
        theme.text.opacity(0.92),
        theme,
    );
    let content_width = match code_width {
        DiffCodeWidth::Clipped => None,
        DiffCodeWidth::Scrollable(metrics) => Some(metrics.unified_content_width(gutter_px)),
        DiffCodeWidth::Wrapped => None,
    };
    let wrapped = matches!(code_width, DiffCodeWidth::Wrapped);
    div()
        .map(|el| {
            if wrapped {
                el.min_h(px(DIFF_LINE_HEIGHT))
            } else {
                el.h(px(DIFF_LINE_HEIGHT))
            }
        })
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_start()
        .when_some(row_bg, |el, bg| el.bg(bg))
        // Accent bar: solid colour on +/− rows, invisible spacer on
        // context rows so columns always align.
        .child(
            div()
                .w(px(ACCENT_BAR_WIDTH))
                .self_stretch()
                .flex_none()
                .when_some(accent, |el, color| el.bg(color)),
        )
        .child(gutter(
            line.old_no,
            if line.kind == LineKind::Del {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(gutter(
            line.new_no,
            if line.kind == LineKind::Add {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(DIFF_TEXT_SIZE))
                .line_height(px(DIFF_LINE_HEIGHT))
                .text_color(marker_color)
                .font_family(theme.font_mono.clone())
                .child(SharedString::from(marker)),
        )
        .child(code_text_viewport(
            line.text.clone(),
            runs,
            theme,
            UNIFIED_CODE_PADDING_LEFT,
            content_width,
            wrapped,
            scroll,
        ))
        .into_any_element()
}

/// `\ No newline at end of file` and friends: a note about the row rather
/// than code, so it is indented past the columns and never tinted. In split
/// mode it spans both halves.
pub(super) fn meta_line_row(text: &str, theme: &Theme, pad_left: f32) -> AnyElement {
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .pl(px(pad_left))
        .text_size(px(10.5))
        .text_color(theme.text_faint)
        .italic()
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// The expanded body of one file section: notices, hunk headers, +/-/context
/// lines with a coloured accent bar, dual line-number gutters, a marker
/// column, and paint-only syntax runs (zeron checkout-diff-sidebar).
/// Shared with the transcript's tool-diff detail blocks — the same component
/// renders a checkout diff section and an inline ACP tool diff. (The changes
/// pane itself virtualizes these rows individually; this stacked form serves
/// the transcript and the fold tween's clipped stand-in.)
/// Full-document old/new highlighting for tool and checkout diffs.
pub(crate) fn render_file_body_with_syntax(
    file: &FileDiff,
    highlights: Option<Arc<DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let gutter_px = gutter_width(file);
    for notice in file_notices(file) {
        children.push(notice_row(notice, theme));
    }
    for hunk in &file.hunks {
        children.push(hunk_header_row(&hunk.header, theme));
        for line in &hunk.lines {
            let spans = highlights
                .as_deref()
                .map(|highlights| highlights.spans(line))
                .unwrap_or(&[]);
            children.push(diff_line_row(
                line,
                spans,
                theme,
                gutter_px,
                DiffCodeWidth::Clipped,
                None,
            ));
        }
    }
    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}
