# Windows verification history

Archived from the development guide during PR cleanup on 2026-09-11.
These are historical observations, not fresh verification of the current tree.
For current setup and commands, see [Windows development](../reference/windows-development.md).

The temporary renderer patch described below has since been replaced by the
merged [Zui fix](https://github.com/zeronsh/zui/pull/7). References to a local
Cargo patch describe the historical test setup, not the current dependency graph.

# Windows development foundation

Status: **source-build development only, not a supported Windows release**. This
continues the [Windows support research](../research/windows-support.md); the
original research remains a historical record of the unmodified source probes.

## Everyday development (no release rebuild)

From the repository root:

```powershell
cargo run --locked -p zeron
```

This uses incremental debug compilation, then opens the desktop app. The first
run can still take time to compile dependencies; this is not hot reload.
To open an already-built application without invoking Cargo:

```powershell
.\target\release\zeron.exe
# Or, if a debug binary has already been built:
.\target\debug\zeron.exe
```

Existing binaries do not include subsequent source edits.

### Codex discovery follow-up

The current Windows resolver accepts native `.exe` files, not global npm
`codex.cmd` / `codex.ps1` wrappers. The local NVM install exposes only those
wrappers on PATH, while its native Codex 0.153.3 binary exists under
`C:\nvm4w\nodejs\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe`.
A direct `--version` probe succeeds; authenticated app-server behavior has not
been verified in this follow-up.

The reference checkout available here is `../laplus-next`, not `../laplus`.
Its server `process.rs` searches PATH with PATHEXT (including `.cmd`), and
`codex.rs` launches explicit PowerShell scripts through PowerShell. Zeron's
batch rejection is intentional to preserve shell-free argument transport;
copying wrapper acceptance alone would bypass that design.

Implemented in source: Windows discovery now finds nested and hoisted native
Codex npm platform-package payloads under searched PATH/NVM prefixes.
`CodexHarness::installed()` now uses the launch resolver, including
`CODEX_EXECUTABLE` and batch-override rejection. Three regression scenarios failed
before the fix and pass afterward; the real-machine installed-signal probe also
passes. Legacy vendor layouts and other package managers remain follow-up work.

The existing executable is unchanged: close the app and any foreground engine,
then use `cargo run --locked -p zeron` to compile the fix incrementally and launch.
No full application build or rendered-picker verification was performed.

Earlier PATH workaround for the old binary (the user reported it did **not**
restore the logo; prefer the fixed source and dev launch above):

```powershell
$codexBin = 'C:\nvm4w\nodejs\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin'
$env:PATH = "$codexBin;$env:PATH"
.\target\release\zeron.exe
```

This changes only the current PowerShell environment. The binary path and CLI
version were verified, but picker visibility and a real provider session still
need a GUI check; the existing executable may predate the current source.


## Build and test

Use native Windows x64, the MSVC Rust toolchain, Visual Studio C++ build tools,
Windows SDK, CMake, and Git for Windows. The repository follows stable Rust.

```powershell
cargo build --release --locked -p zeron
cargo test --release --locked -p zeron
cargo test --locked -p zeron-engine -p zeron-update --lib
```

The pinned GPUI build script compiles DirectX shaders in release builds. It finds
`fxc.exe` through `GPUI_FXC_PATH`, PATH, or Windows SDK discovery. If discovery
fails, set `GPUI_FXC_PATH` to the **full executable path**, not its directory.

[Windows foundation CI](../../.github/workflows/windows.yml) adds release linking,
application/updater tests, engine and UI library suites, shader-layout tests, a native
rendering fixture build, and an isolated `zeron status` smoke with `HOME` absent.
Its optional `workflow_dispatch` input `native_gui` runs the native probes below
when the runner has a usable desktop/Direct3D device. It does not publish artifacts
or establish general agent/installer support. It has not yet run on GitHub Actions.

## Native rendering and lifecycle probes

```powershell
cargo build --release --locked -p zeron-ui --example windows-render-fixture --features windows-render-fixture
cargo test --release --locked -p gpui_windows --lib layout_tests
./scripts/test-windows-lifecycle.ps1 -Runs 5
./scripts/test-windows-rendering.ps1
```

Run on an interactive desktop. The lifecycle probe isolates the application and
provider home directories under `target/`, closes the actual application window,
requires exit 0, and reopens the same profile with a stable device identity. The
rendering fixture has no engine or user data. Its script captures only its own
client HWND using `PrintWindow`, then asserts pixel colors/gradients and text.
Capture runs in a helper process bounded to 15 seconds so a hung renderer cannot
block cleanup. A deliberately unresponsive synthetic WinForms window exercised
this timeout path successfully.
Synthetic PNGs, captures, measurements, and logs remain in unique ignored
`target/windows-render-*` / `target/windows-lifecycle-*` directories.

The root Cargo patch replaces **only** `gpui_windows` with a project-owned copy.
All other GPUI crates retain the original revision and dependency identity. See
[upstream renderer fix](https://github.com/zeronsh/zui/pull/7). This fixes mismatched
Rust/HLSL structured-buffer fields/strides and ports the pinned fork's quad/image
edge fades. It does not implement Windows backdrop blur.

Captured evidence at 96 DPI: [original renderer](../reference/assets/windows-render-before.png)
versus [patched renderer](../reference/assets/windows-render-after.png). The patch restores
missing colored quads and correct atlas colors; the extra bottom row checks
asymmetric vertical fades.

The four layout tests assert CPU sizes/offsets and HLSL field order/use. They are
not compiled-shader reflection tests; native pixel assertions complement that
source-level guard. Future shader ABI changes should add reflection checks too.

## Storage and updates

- `ZERON_DATA_DIR` overrides the application data root.
- Native Windows defaults to `%LOCALAPPDATA%\Zeron`, falling back to
  `%USERPROFILE%\AppData\Local\Zeron`. Shell-specific `HOME` does not change this.
- Provider credential locations are separate; this is **not** a credential or
  existing Unix-style Windows data migration.
- Engine ownership uses a Windows OS file lock. `engine.lock.pid` is only a
  diagnostic sidecar; a stale PID is not evidence of an active engine.
- Windows installations are report-only/unmanaged for updates, even if copied
  into a Unix-installer-shaped directory. Windows is no longer mislabeled Linux.
  Unix symlink replacement/service restart and macOS bundle replacement reject
  Windows before those operations have side effects. Rebuild from source to
  update this development build.

## Native agents and ConPTY foundation

```powershell
cargo test --locked -p zeron-harness --lib
cargo test --locked -p zeron-harness --features native-fixture --test windows_native
cargo test --locked -p zeron-engine --lib
```

Windows discovery shares a native-`.exe` resolver across ACP, Claude and Codex.
It ignores empty PATH entries, directories, and shell-only shims. Explicit native
executable overrides keep precedence. Windows batch overrides are rejected with
native-executable guidance rather than exposing dynamic arguments to `cmd.exe`.

Managed adapters use `ZERON_ADAPTERS_DIR` first, then `ZERON_DATA_DIR/adapters`,
then `%LOCALAPPDATA%/Zeron/adapters` or the `USERPROFILE` fallback. Unix retains
`~/.zeron/adapters`. Windows installation supports a standard Node layout with
`node.exe` beside `node_modules/npm/bin/npm-cli.js`: it invokes Node with exact
arguments, not `npm.cmd`. Managed PE binaries launch directly, while JavaScript
entries launch through Node. Managed batch entries are rejected.

**Known discovery gaps:** ordinary global npm `.cmd` wrappers are not resolved to
their underlying package entries yet. Volta's selected tool-image npm location is
not resolved through its `bin/node.exe` shim. Thus an agent working in a shell is
not yet sufficient to establish that Zeron discovers it. No general npm/Volta or
real-provider support claim is made by these changes.

The synthetic Rust ACP peer tests real Windows stdio, a Unicode/metacharacter
executable path and cwd, exact prompt transport, resume identity, and cooperative
protocol cancellation with direct-child exit. It never authenticates or installs
an agent. This is not evidence of unresponsive-agent or descendant-tree cleanup.

Real ConPTY tests cover Unicode output/cwd, observable resize, input, full replay,
after-sequence replay, natural exit, close, and final-owner drop. They reproduced
an EOF deadlock: ConPTY held output open until its owning pseudoconsole handles
were released. The pump now observes child exit independently, releases those
handles, drains final output, and emits `Exit`. Final terminal-owner destruction
also kills live direct children. Detached descendants still require process-tree
ownership; reader threads do not yet have an awaitable shutdown acknowledgement.


## Local verification, 2026-09-10

Host: Windows x64/MSVC, Rust 1.97.0; workspace 0.2.54, GPUI revision unchanged at
`07fd941ad72e7edc812fed317aab66adb69fa8cc`, with the local Windows-backend patch.

| Probe | Result |
| --- | --- |
| `cargo check --locked -p zeron-engine --tests` | Passed. |
| `cargo build --release --locked -p zeron` | Passed; native executable linked and generated release shader headers present. |
| `cargo test --release --locked -p zeron` | Passed, 11 tests. |
| `cargo test --locked -p zeron-engine -p zeron-update --lib` | Passed, 162 engine and 7 updater tests. Includes Windows subprocess lock contention/release and source-control PowerShell fixtures. |
| Updater Windows-target check and clippy with warnings denied | Passed in a separate local target directory. |
| Release `zeron status`, no `HOME` or `ZERON_DATA_DIR`, isolated `LOCALAPPDATA` containing spaces, apostrophe, and Japanese characters | Passed: correct native data root, local-only/signed-out state, no engine connection (IPC port zero). |
| Combined release application + engine + updater tests, including integration targets | Hit the local 600-second command limit during compilation/linking; not a passing suite result. The separate checks above are the verified alternative. |
| Native lifecycle probe | Passed five consecutive open/close/reopen cycles, exit 0, stable device identity. |
| Native rendering pixel probe | Failed against the original backend (missing quads/corrupt atlas); passed with the patch for four PNG quadrants, horizontal and asymmetric vertical quad/image fades, text, and an opaque panel. Captured on NVIDIA RTX 5050 / Direct3D 11.1 at 96 DPI. |
| `cargo test --release --locked -p gpui_windows --lib layout_tests` | Passed, four CPU/HLSL layout and fade-use regressions. |
| `cargo test --release --locked -p zeron-ui --lib -- --test-threads=1` | Passed, all 758 tests, after correcting failed-bootstrap cleanup and remote POSIX-link resolution. |
| `cargo test --locked -p zeron-harness --lib` | Passed, 113 tests, including native resolution, batch rejection, no-HOME roots, PE detection and shell-free npm launch plans. |
| `cargo test --locked -p zeron-harness --features native-fixture --test windows_native` | Passed, four native Windows regressions: stdio/prompt/path fidelity, session load, cooperative cancellation/direct-child exit, and pre-spawn batch-override rejection across ACP/Claude/Codex. |
| `cargo test --locked -p zeron-engine --lib` after native-launch integration | Passed, 164 tests, including two real PowerShell/ConPTY lifecycle regressions. |
| Final native-agent/ConPTY release build and GUI smoke | Release build passed on retry after the first 600-second compile/link timeout; rebuilt executable passed three additional clean native open/close cycles. |

Failures found and corrected during continuation:

1. The ongoing application path refactor left `update_cli` calling the removed
   `dirs_data_dir` symbol; it now uses the shared `paths::data_dir` resolver.
2. A staging-sweep test opened a directory with ordinary `File::open`, which fails
   with Windows access denied. Its timestamp fixture now opens a directory handle
   with backup semantics and write-attributes access. The fresh/stale directory
   assertions are unchanged.
3. An installer-source test assumed an LF checkout. It now normalizes CRLF before
   checking the same service directives, so Git for Windows does not cause a
   false failure.
4. Updater regressions reproduced Windows being labeled Linux and classified as
   a managed Unix installation; platform guards and report-only detection fix both.
5. The renderer's HLSL omitted CPU `EdgeFadeParams`, so later quad instances and
   image atlas metadata were read at the wrong offsets. The scoped backend patch
   restores the ABI and implements the existing four-edge fade semantics.
6. Full UI tests exposed failed-bootstrap IPC cleanup racing task cancellation.
   Shutdown now awaits the aborted IPC task before returning; the existing test's
   immediate closed-listener assertion is preserved.
7. Remote POSIX absolute links failed in Windows viewports because `is_absolute`
   requires a Windows drive. Rooted-link classification now uses `has_root`,
   retaining containment and unsafe-path rejection.

The earlier startup/path, instance-lock, and source-control edits were already in
progress when this continuation began and were preserved. There is no commit,
installation, credential change, remote dependency-pin change, or published Windows
build. The local Cargo patch is intentional and recorded in the lockfile.

## Remaining acceptance gates

The first research gate now has native rendering evidence, but is **not fully
accepted** across the proposed validation matrix.

- The CPU/HLSL discrepancy is fixed and native pixels pass at 96 DPI. Still test
  real popovers/overlays, image clipping/rounding, multiple monitor DPIs and DPI
  transitions, additional GPU drivers, and accessibility. No backdrop-blur parity
  is claimed.
- The earlier GUI-close failure was a **probe defect**, not a confirmed app hang:
  Windows PowerShell's `Start-Process` could report a null `ExitCode` without an
  early retained process handle. The corrected probe passes. GPUI already quits
  on the last Windows window and preserves macOS reopen behavior; no speculative
  `cx.quit` callback was added. Invalid-HWND teardown logs still deserve investigation,
  and an idle close/reopen test does not prove durable shutdown during live runs.
- Audit Windows log rotation under concurrent launches; its non-Unix path still
  lacks the Unix ownership guard. Empty/stale lock PID sidecars affect diagnostics;
  normalize missing PID text without treating it as liveness.
- Complete global npm wrapper and Volta-image discovery, test one real provider's
  authenticated lifecycle, and implement/test unresponsive-agent and descendant
  process-tree termination. Native synthetic ACP and direct-child ConPTY checks
  now pass, but do not establish those stronger guarantees.
- Validate cross-device paths/sync, persistence, installer identity, background
  startup, graceful stop, update/rollback, and native browser/accessibility parity.

Use isolated data roots for all further probes. Do not point lifecycle, login,
logout, or migration tests at an existing Zeron profile or provider credentials.
