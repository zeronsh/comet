# Agent worklog

## 2026-08-22 — Theme import experience

### Product goal

Make adding a custom theme feel like a short settings task rather than a command-palette workflow, while keeping source compilation, variant selection, snapshot/link installation, and optional mapping review intact.

### Control section inventory

- Source
  - Local file/package/folder field
  - Browse action
- Installation
  - Snapshot or linked-source choice
- Detected themes
  - Variant selection
  - Palette preview
  - Optional mapping details
- Completion
  - Cancel
  - Analyze or import primary action
- Installed theme
  - Reload when linked
  - Reveal source
  - Review mapping
  - Duplicate as editable
  - Unlink when linked
  - Remove

### Decisions

- Use a conventional, compact modal with a title, supporting copy, body, and footer actions.
- Remove command-palette chrome, keyboard-hint footer, duplicated primary actions, and the permanent side rail.
- Keep snapshot/link as an inline segmented choice with concise explanations.
- Keep mapping details collapsed by default; show them only when requested for a variant.
- Render installed-theme actions as bordered controls rather than bare text.
- Preserve theme compilation, persistence, settings transfer, source linking, and runtime selection behavior.
- No timeline, layers, export changes, custom renderer, or new persistence are required.

### Implementation plan

- Update `crates/ui/src/settings/appearance.rs` to simplify the import dialog layout, make import errors visible in the body/footer, collapse technical mapping by default, and restyle installed-theme actions.
- Add or adjust focused unit coverage for any extracted presentation helpers where practical.
- Run `cargo fmt --check`, the relevant `zeron-ui` tests/checks, and a local app build.
- Verify the settings page and both import states in the local app browser/UI at the desktop viewport shown in the supplied screenshots.

### Verification tier

Tier 2: compile and test the affected Rust UI crate, then visually inspect the settings page plus empty and analyzed import states. The change is UI-only and does not alter compilation or persistence formats.

### Verification result

- `cargo fmt --check` passes.
- `cargo check -p zeron-ui` passes with existing workspace warnings.
- `cargo test -p zeron-ui --lib settings::appearance` passes (4 tests).
- A packaged debug build was opened on macOS and the Appearance page, installed-theme row, and empty import state were visually inspected at 1365 × 768.
- The final visual pass found and removed a remaining overflowing helper sentence; long error paths are now explicitly truncated within the dialog.

## 2026-08-20 — Appshots exploration

- Product goal: capture the frontmost application from a global shortcut and
  stage screenshot plus semantic application context in a Zeron composer.
- Visible output: a distinct, removable Appshot card in the destination draft;
  the existing paperclip and ordinary attachment flows remain unchanged.
- Editable entities: global shortcut, destination policy, staged Appshot, and
  the user's accompanying prompt.
- Required controls: documented by workflow in
  `docs/research/appshots.md#control-section-inventory`.
- Export behavior: none. A send reuses Zeron's attachment upload and prompt
  transport.
- Persistence: device-local settings; unsent-capture persistence remains an
  explicit product decision.
- Layers/timeline/custom renderer: not required.
- Verification tier: native macOS spike, pure routing/serialization tests,
  local and remote integration tests, and a manual permissions/application
  matrix.
- Main architecture decision: desktop capture is viewer-side UI capability,
  never an engine or remote-host responsibility.

## 2026-08-20 — Appshots implementation and macOS hardening

- Implemented the complete macOS capture-to-composer slice on
  `wip/appshots-exploration` with `Control-Option-Space` as the fixed default.
- Replaced the initial event tap with Carbon global-hotkey registration, which
  consumes the chord and does not require Input Monitoring permission.
- Uses ScreenCaptureKit's desktop-independent window capture on macOS 14+;
  retains a CoreGraphics fallback for macOS 12/13 and transient failures.
- Window enumeration is no longer restricted to the current Space. Active
  ScreenCaptureKit windows win, followed by on-screen state and window area.
- Added an isolated `Zeron Dev.app` workflow with bundle ID
  `sh.zeron.app.dev`, stable Apple Development signing, its own data directory,
  and IPC port. It launches through LaunchServices so TCC attributes capture
  permissions to Zeron Dev rather than the terminal.
- The isolated data directory and IPC port are embedded in the development
  bundle's `LSEnvironment`, so macOS privacy's “Quit & Reopen” preserves the
  development instance instead of reopening against personal production data.

## 2026-08-20 — Appshots composer and onboarding refinement

- Product goal: make staged Appshots visually scannable without consuming the
  composer and make the macOS permission sequence understandable before any
  system prompt appears.
- Visible output: one horizontal tray of 232×148 Appshot tiles. Each tile uses
  a contained window preview, the captured application's icon over the lower
  edge, a concise window/app label, hover removal, and the existing lightbox.
- Permission workflow: enabling Appshots only enables the feature. Screen
  Recording is the single required capture permission; Accessibility is an
  optional enhancement for application text, including off-screen content.
  The two permissions have separate status rows and user-triggered actions.
- Controls: feature toggle, destination selector, required Screen Recording
  action, optional Accessibility action, and a permission-status refresh.
- Persistence and transport are unchanged. Application icons are presentation
  metadata only and are never uploaded or serialized into prompts.
- Verification tier: pure tray-height and prompt tests, full `zeron-ui` unit
  suite, macOS compile/build, and native visual/permission smoke testing.

## 2026-08-21 — Appshots aspect-ratio and fade refinement

- Product goal: make every Appshot read as one consistently sized composer
  object even when source windows range from very wide to very tall.
- Visible output: a fixed, clipped stage containing a bottom-aligned image,
  with explicit aspect-ratio sizing, a native bottom edge-fade, the app icon
  floating above that fade, and more air before the title.
- Control inventory is unchanged: the tile still opens the lightbox and its
  hover action removes it. No new settings, persistence, transport, export,
  layer, or timeline behavior is introduced.
- Renderer decision: retain the native GPUI image renderer, but stop depending
  on its intrinsic sizing. Read dimensions from the captured PNG and assign
  exact contained dimensions inside both preview-level and card-level clips.
- Verification tier: pure PNG/aspect-ratio tests, full `zeron-ui` unit suite,
  macOS build checks, and native composer inspection with landscape and
  portrait captures.

## 2026-08-22 — Windows and Linux Appshots

- Product goal: extend the existing capture-to-composer contract to Windows,
  Linux X11, and Linux Wayland without presenting their security models as if
  they were macOS permissions.
- Visible output and editable entities are unchanged after capture. Settings
  now inventory three workflow capabilities: global invocation, window capture
  (including whether selection is required), and semantic application text.
- Backend decision: a UI-side `AppshotBackend` owns native initiation and
  capture. Linux selects Wayland portal or X11 at runtime; AT-SPI enrichment is
  independent so screenshot-only Appshots remain useful.
- Windows decision: use a thread-owned `RegisterHotKey`, foreground HWND,
  Windows Graphics Capture, and bounded UI Automation. Do not request the
  security-sensitive `UIAccess` privilege.
- Wayland decision: probe portal versions and advertised targets. Prefer
  Active Window; fall back to the portal window picker. Prefer the Global
  Shortcuts portal and expose `zeron appshot` for system-managed shortcut
  configuration when the portal is absent.
- X11 decision: prefer a portal-advertised Active Window target, then use
  `_NET_ACTIVE_WINDOW`, a passive key grab, direct drawable capture, EWMH
  process/name/icon metadata, and the same optional AT-SPI layer.
- Persistence, transport, composer controls, export, layers, and timeline are
  unchanged. Verification combines pure platform parsing tests, host checks,
  cross-target compilation where available, and native Windows/GNOME/KDE/X11
  release matrices.
