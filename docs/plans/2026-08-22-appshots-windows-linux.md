# Windows and Linux Appshots implementation plan

## Behavior and architecture

1. Refactor `crates/ui/src/appshots.rs` behind an `AppshotBackend` trait and a
   platform-neutral capability model. Shortcut readiness, active-window
   capture, semantic text, and target selection are independent states; Linux
   is selected at runtime as Wayland portal or X11.
2. Add `crates/ui/src/appshots/windows.rs` using `RegisterHotKey`,
   `GetForegroundWindow`, Windows Graphics Capture, bounded UI Automation,
   process metadata, and executable icons. Elevated/protected targets degrade
   to screenshot-only or a concrete capture error; Zeron does not request
   `UIAccess`.
3. Add `crates/ui/src/appshots/linux/{mod,portal,x11,atspi}.rs`. Wayland uses
   the Screenshot portal's Active Window target when advertised, otherwise its
   window picker; the Global Shortcuts portal is preferred. On X11, an
   advertised portal Active Window target is also preferred before an EWMH
   active window, passive key grab, direct drawable capture, window metadata,
   and `_NET_WM_ICON`. AT-SPI enrichment is bounded and optional for both.
4. Add a Linux-only `zeron appshot` activation command and local activation
   socket for desktops without the Global Shortcuts portal. It activates the
   already-running headed viewport; it never starts a headless capture.
5. Rework `crates/ui/src/settings/shortcuts.rs` to render capabilities rather
   than macOS permission assumptions. macOS retains explicit Screen Recording
   and Accessibility actions; Windows shows ready states; Wayland explains
   portal selection/setup; X11 reports native readiness.

## Unchanged surfaces

- Composer layout, routing, uploads, prompt serialization, remote delivery,
  destination persistence, and staged-review semantics stay authoritative.
- No schema migration, timeline, layer model, export path, or paperclip change.
- Screenshot bytes remain ordinary staged PNG attachments; platform metadata
  remains presentation/prompt context.

## Verification

- Pure tests for capability copy, PNG/pixel conversion, bounds, X11 property
  parsing, and activation-path derivation.
- `cargo fmt --all -- --check`, `cargo test -p zeron-ui --lib`,
  `cargo check -p zeron-ui`, and `cargo check -p zeron` on macOS.
- Target checks for Windows and Linux where the local toolchain/sysroot allows;
  otherwise record the exact missing machine dependency and keep all platform
  code target-gated for CI/native validation.
- Native release gates: Windows 10 1903+/11 (normal/elevated/protected and DPI
  matrix), GNOME/KDE Wayland with portal v2/v3 behavior, and X11 with obscured,
  fullscreen, and multi-monitor windows.
