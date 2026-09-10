# Windows development

Windows currently supports source-build development. There is no supported
Windows release, installer, managed updater, or background service.

## Prerequisites and running

Use native Windows x64 with stable MSVC Rust, Visual Studio C++ build tools,
Windows SDK, CMake, and Git for Windows. From the repository root:

```powershell
cargo run --locked -p zeron
```

Cargo builds incrementally and opens the desktop app. Close the app before
rebuilding its executable. To run an existing build:

```powershell
.\target\debug\zeron.exe
# For an existing release build:
.\target\release\zeron.exe
```

Existing binaries do not include subsequent source edits.

## Configuration

| Setting | Behavior |
| --- | --- |
| `ZERON_DATA_DIR` | Overrides the application data root. |
| Default application data | `%LOCALAPPDATA%\Zeron`, falling back to `%USERPROFILE%\AppData\Local\Zeron`. Shell `HOME` does not select this root. |
| Managed adapters | `ZERON_ADAPTERS_DIR`, then `ZERON_DATA_DIR/adapters`, then the default application data root's `adapters` directory. |
| Provider credentials | Keep their own locations; changing Zeron's data root does not migrate credentials or existing Unix-style data. |
| `CODEX_EXECUTABLE` | Optional native executable override. Windows batch overrides are rejected. |
| Updates | Rebuild from source. Windows installations are unmanaged; Unix service/symlink and macOS bundle replacement are unavailable. |

Engine ownership uses an OS file lock. `engine.lock.pid` is diagnostic only;
a stale PID does not establish that an engine is running.

### Frosted appearance

The Frosted surface preference requests native Acrylic for the window chrome
(sidebar and space around the content cards). Windows **Settings >
Personalization > Colors > Transparency effects** must be enabled; when it is
off, Windows disables Acrylic. Zeron does not change this system preference.
See Microsoft's [Acrylic material documentation](https://learn.microsoft.com/en-us/windows/apps/design/style/acrylic).

Compare Frosted and Opaque with the app focused and a colorful window behind
it. Content cards, menus, and popovers remain opaque on Windows: the DirectX
renderer does not implement in-app `BackdropBlur` yet. Native window Acrylic
and in-app blur are separate capabilities.

## Agent discovery and terminals

ACP, Claude, and Codex share native executable discovery. It searches PATH and
known installation directories, ignores empty PATH entries and directories, and
preserves explicit executable overrides. Codex also resolves nested and hoisted
native npm platform-package payloads under searched PATH/Node-manager prefixes.
Availability and launch use the same Codex resolver.

Managed JavaScript adapters launch through Node; native PE binaries launch
directly. Installation requires a Node layout with `node.exe` beside
`node_modules/npm/bin/npm-cli.js`. Arguments are passed directly to Node.
Batch wrappers (`.cmd` / `.bat`) are rejected.

General npm wrapper resolution, older Codex vendor layouts, and Volta's selected
tool-image npm location remain unsupported. A CLI working in a shell does not
by itself establish that Zeron discovers it.

Terminals use ConPTY. The output pump observes child exit independently of EOF,
releases pseudoconsole handles, drains buffered output, then emits the exit event.
Close and final-owner drop terminate the direct child. Descendant process-tree
termination and shutdown acknowledgement from reader threads remain follow-ups.

## Build and test

The Windows workflow runs the following checks. Release linking matters because
the GPUI build script compiles DirectX shaders:

```powershell
cargo build --release --locked -p zeron
cargo test --release --locked -p zeron -p zeron-update
cargo test --release --locked -p zeron-harness --lib
cargo test --release --locked -p zeron-harness --test codex_availability
cargo test --release --locked -p zeron-harness --features native-fixture --test windows_native
cargo test --release --locked -p zeron-engine --lib
cargo test --release --locked -p zeron-engine --test codex_catalog
cargo test --release --locked -p zeron-ui --lib -- --test-threads=1
```

For faster local iteration, omit `--release` from focused crate tests. If shader
compiler discovery fails, set `GPUI_FXC_PATH` to the full path of `fxc.exe` in
the Windows SDK.

The Codex tests verify discovery and the production harness catalog using
synthetic payloads. The native ACP fixture exercises stdio, argument fidelity,
resume, and cooperative cancellation without authenticating or installing an
agent. Engine tests cover file-lock contention and real ConPTY behavior.

[Windows CI](../../.github/workflows/windows.yml) also checks startup without
`HOME` using an isolated data directory. Its optional `native_gui` dispatch
runs the probes below; ordinary PR checks do not run those desktop probes.
Shared engine/harness/updater regression coverage also runs on Linux and macOS.

## Native rendering and lifecycle checks

Run on an interactive Windows desktop:

```powershell
cargo build --release --locked -p zeron-ui --example windows-render-fixture --features windows-render-fixture
cargo test --release --locked -p gpui_windows --lib layout
./scripts/test-windows-lifecycle.ps1 -Runs 5
./scripts/test-windows-rendering.ps1
```

The lifecycle probe uses isolated application and provider homes, closes the
application window, requires exit 0, and reopens the same profile with a stable
device identity. The rendering fixture has no engine or user data. Its capture
helper captures only the fixture's client window and has a 15-second timeout.
Evidence stays in unique ignored `target/windows-render-*` and
`target/windows-lifecycle-*` directories.

The pinned Zui revision includes the [upstream DirectX fix](https://github.com/zeronsh/zui/pull/7)
for CPU/HLSL buffer layouts and quad/image edge fades. All GPUI crates use that
revision directly; no local renderer patch is needed. In-app backdrop blur
remains unsupported on Windows.

Layout tests check CPU offsets and HLSL source declarations, not compiled-shader
reflection. Native pixel checks complement them. Historical before/after images
and test results are in the [verification archive](../research/windows-verification-history.md).

## Remaining limitations

- Windows packaging, background startup, managed updates, and rollback.
- Authenticated real-provider lifecycle and unresponsive/descendant process cleanup.
- Broader GPU and multi-DPI coverage, accessibility, and native browser parity.
- Windows backdrop blur, cross-device acceptance, and shutdown during live runs.
- Concurrent-launch log rotation and invalid-window teardown diagnostics.

Use isolated data roots for lifecycle and migration probes. See the
[original investigation](../research/windows-support.md) for historical context.
