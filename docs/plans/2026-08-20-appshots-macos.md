# macOS Appshots implementation plan

## Behavior

Implement a macOS-only global shortcut that captures the frontmost application
window before Zeron activates, collects bounded Accessibility context, routes
the result to the current or new-session composer, and stages it for review and
send. The paperclip and ordinary image intake remain unchanged.

## Files and boundaries

- `crates/ui/src/appshots.rs`: platform-neutral capture model, prompt
  serialization, routing helpers, and macOS service boundary.
- `crates/ui/src/appshots/macos.rs`: ScreenCaptureKit capture with a bounded
  CoreGraphics fallback for older macOS versions, Accessibility traversal,
  permission probes, and Carbon global-hotkey registration.
- `crates/ui/src/lib.rs`: initialize and retain the capture service; deliver
  captures to the main window without capturing Zeron itself.
- `crates/ui/src/settings.rs`: persist Appshots enabled/destination/first-use
  state as device-local settings.
- `crates/ui/src/settings/shortcuts.rs`: expose Appshots enablement,
  destination, permission status, and the fixed macOS shortcut alongside app
  shortcuts.
- `crates/ui/src/shell.rs` and `crates/ui/src/shell/tabs.rs`: route a completed
  capture to the selected composer or new-session canvas and restore chat UI.
- `crates/ui/src/composer.rs`: own Appshots per draft, render distinct staged
  cards, remove/preview them, include them in send eligibility, serialize
  semantic context, and restore them after failed sends.
- `crates/ui/src/attachments.rs`: construct a staged PNG from native bytes and
  keep existing upload/path behavior authoritative.
- `crates/ui/Cargo.toml`: macOS framework/FFI dependencies only where needed.
- `docs/research/appshots.md` and `docs/toolcraft/agent-worklog.md`: record any
  implementation decisions that differ from the exploration.

No schema migration, timeline, layers, renderer replacement, or export path is
required. Screenshot files continue through `RunRequest.attachments`; the
semantic Appshot block is generated into `RunRequest.prompt` at the UI send
boundary so existing local and remote attachment delivery remains intact.

## Verification

- Unit tests for XML escaping, context limits, image/path association,
  Appshot-only send eligibility, and destination decisions.
- Existing settings round-trip tests extended for backward-compatible defaults.
- Composer send/failure tests extended where practical for Appshot state.
- `cargo fmt --all -- --check`.
- `cargo check -p zeron-ui`.
- `cargo test -p zeron-ui`.
- A headed macOS smoke run with an isolated `ZERON_DATA_DIR` to validate global
  shortcut delivery, permission recovery, frontmost-window ordering,
  screenshot preview, accessibility degradation, and local send. Remote-path
  behavior is covered by the existing queued attachment transport plus focused
  serialization/path-rewrite tests.

## Performance and safety limits

- Capture work runs off the GPUI render path.
- Accessibility traversal is bounded by depth, node count, text size, and a
  short deadline; partial output is marked truncated.
- Screenshot dimensions and encoded bytes reuse attachment limits.
- ScreenCaptureKit output is Retina-scaled and capped to 4096 px on its longest
  edge before entering the attachment pipeline.
- Accessibility payloads are never logged and secure text values are skipped.
- Global callbacks enqueue work only; Objective-C/CoreFoundation ownership is
  contained in the macOS module.
