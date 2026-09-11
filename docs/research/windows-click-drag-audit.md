# Windows click/drag audit

Date: 2026-09-11

## Question

Which Comet controls, beyond the right-sidebar agent tabs, combine click activation with GPUI drag-and-drop and can therefore turn small Windows pointer movement into a drag instead of a click?

## Conclusion

The same failure mode exists in three other user-facing control families: terminal tabs, file-tree rows, and file-search result rows. They attach `on_click` and `on_drag` to the same full control. All use GPUI's global two-pixel drag threshold, and starting a drag explicitly clears GPUI's pending clicked state. A mouse movement whose Euclidean distance is greater than 2 logical pixels therefore suppresses activation and shows a drag ghost.

The terminal tabs are the closest match to the reported agent-tab bug and should be fixed in the same change. File tree and search results are also exposed, but preserving their intentional external file drag behavior may require a Windows-only drag handle or an upstream configurable threshold instead of simply removing drag.

No other production `on_drag` site combines ordinary left-click activation with the same draggable hitbox. Resize handles and scrollbars are intentional continuous-drag controls; the queue row is draggable but not row-clickable; history column headers are draggable but have no left-click action.

## Root mechanism

Comet pins GPUI/Zui revision `3151ad1` in [`Cargo.toml`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/Cargo.toml#L67-L74). At that revision, GPUI defines `DRAG_THRESHOLD` as `2.0` and starts a drag when pointer displacement is **greater than** that threshold ([Zui `div.rs`](https://github.com/zeronsh/zui/blob/3151ad1dbd9187bf47e716d4735d811f1055ed1d/crates/gpui/src/elements/div.rs#L48), [drag initiation](https://github.com/zeronsh/zui/blob/3151ad1dbd9187bf47e716d4735d811f1055ed1d/crates/gpui/src/elements/div.rs#L2766-L2785)). Drag initiation resets `clicked_state` and removes the pending mouse-down, while click listeners fire only if that pending mouse-down survives until mouse-up ([click dispatch](https://github.com/zeronsh/zui/blob/3151ad1dbd9187bf47e716d4735d811f1055ed1d/crates/gpui/src/elements/div.rs#L2857-L2893)). This is direct evidence for the lost-click behavior, not merely visual overlap with the drag ghost.

The upstream Zed report [#58970](https://github.com/zed-industries/zed/issues/58970) independently documents one-pixel pointer jitter during a Windows-focusing click as a real input pattern. That issue concerns terminal selection rather than Comet tabs, so it supports the Windows-jitter premise but is not evidence that GPUI's two-pixel threshold is itself Windows-specific. The threshold and cancellation mechanism are cross-platform; the present report says the triggering input has only been observed on Windows.

## Prioritized risk list

### P0 — right-sidebar surface/agent tabs

- Full tab click switches the right surface at [`shell.rs:7555`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/shell.rs#L7555-L7559).
- The same full tab starts `RightTabDrag` at [`shell.rs:7567`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/shell.rs#L7567-L7579).
- Impact: the reported primary navigation action does not happen and the surface follows the pointer.
- Recommendation: on Windows, do not register the full-tab drag until GPUI exposes a suitable configurable threshold. Preserve click, middle-click close, and explicit close-button behavior.

### P0 — terminal tabs

- Full tab click selects/focuses a terminal at [`terminal/panel.rs:1531`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/terminal/panel.rs#L1531-L1534).
- The same full tab starts `TabDragPayload` at [`terminal/panel.rs:1542`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/terminal/panel.rs#L1542-L1553).
- Impact: switching terminal sessions can fail in exactly the same way as switching right-sidebar agents.
- Recommendation: apply the same Windows policy and regression test as right-sidebar tabs.

### P1 — file tree rows

- Full row click focuses the tree and activates the path at [`files/tree.rs:150`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/files/tree.rs#L150-L153).
- The same row starts a workspace-path drag at [`files/tree.rs:154`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/files/tree.rs#L154-L157).
- Impact: opening a file or expanding a directory can instead produce a file drag ghost.
- Recommendation: retain file dragging, but on Windows move initiation to a dedicated drag affordance or gate it behind a larger local threshold once the framework supports one. If immediate reliability is preferred, temporarily disable row drag on Windows.

### P1 — file-search result rows

- Full result click selects and activates the result at [`files/search.rs:549`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/files/search.rs#L549-L552).
- The same result starts a workspace-path drag at [`files/search.rs:553`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/files/search.rs#L553-L556).
- Impact: opening a search result can become an unintended file drag.
- Recommendation: use the same policy as file-tree rows so identical workspace-path controls behave consistently.

### P2 — double-click resize reset

Three resize seams combine drag with a double-click reset: the main pane seam ([`shell.rs:6548-6560`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/shell.rs#L6548-L6560)), terminal height seam ([`shell.rs:6956-6968`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/shell.rs#L6956-L6968)), and history column resize handle ([`history.rs:3004-3019`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/history.rs#L3004-L3019)). Jitter may prevent a double-click reset, but these narrow controls are primarily drag handles, so changing them would harm their main interaction. Leave them unchanged unless Windows users report failed double-click resets.

## Audited and not click-conflicted

The production audit found 13 `on_drag` call sites (excluding a documentation example). The remaining sites do not share a normal left-click activation action:

- History column headers reorder on left-drag and only the Author header handles right mouse-down for a menu ([`history.rs:3069-3090`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/history.rs#L3069-L3090)).
- Queue rows are full-row reorder targets, but row activation is absent; actions are separate child buttons ([`queue.rs:448-478`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/queue.rs#L448-L478)).
- Composer popup, model-picker, and Markdown code scrollbars use mouse-down plus drag as one continuous scrollbar interaction ([`composer.rs:5173-5194`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/composer.rs#L5173-L5194), [`pickers.rs:2786-2807`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/pickers.rs#L2786-L2807), [`markdown/render.rs:1443-1468`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/markdown/render.rs#L1443-L1468)).
- The file-preview split seam is drag-only ([`files/preview.rs:2668-2687`](https://github.com/zeronsh/comet/blob/a48e1fb2347657ab3afd9ba3661da4968559438e/crates/ui/src/files/preview.rs#L2668-L2687)).

## Verification target

For each P0/P1 control, add a Windows regression covering mouse-down, approximately 3–5 px of movement, and mouse-up. It should assert that the control's activation occurs and that `cx.has_active_drag()` remains false. Separately retain an intentional reorder test on platforms where drag stays enabled. The current right-tab regression deliberately moves eight pixels and expects an active drag, so it documents the behavior that needs a Windows-specific expectation rather than protecting click tolerance.

## Implemented resolution

Comet now centralizes this distinction in `click_activation_drag_enabled()`. On Windows, the four P0/P1 control families above no longer register drag initiation on their click-activation hitboxes. Their primary click actions therefore survive pointer jitter. Drag-first controls remain unchanged, and macOS/Linux retain tab reordering and workspace-path dragging.

The production right-tab visual regression reproduces an eight-pixel moving click. It failed before the change because GPUI entered an active drag, then passed after the Windows policy was applied. The complete `zeron-ui` library suite passed with 789 tests.
