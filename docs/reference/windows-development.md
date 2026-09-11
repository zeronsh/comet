# Windows development

Windows supports native x64 source builds and portable release ZIPs. Release
packages offer in-app updates through GitHub; keep `zeron-update.json` beside
`zeron.exe`. Installers and background services are not supported yet.

## Build and run

Install stable MSVC Rust, Visual Studio C++ build tools, Windows SDK, CMake,
and Git for Windows, then run:

```powershell
cargo run --locked -p zeron
```

Close the app before rebuilding. For release builds, use
`cargo build --release --locked -p zeron`. If shader compiler discovery fails,
set `GPUI_FXC_PATH` to the Windows SDK's `fxc.exe`.

## Configuration and agent support

| Setting | Behavior |
| --- | --- |
| Application data | `%LOCALAPPDATA%\Zeron`, falling back to `%USERPROFILE%\AppData\Local\Zeron`. Override with `ZERON_DATA_DIR`. |
| Managed adapters | `ZERON_ADAPTERS_DIR`, then `ZERON_DATA_DIR/adapters`, then the default application's `adapters` directory. |
| Provider credentials | Keep their provider-owned locations; changing Zeron's data root does not migrate them. |
| `CODEX_EXECUTABLE` | Native executable override; `.cmd` and `.bat` wrappers are rejected. |

ACP, Claude, and Codex search PATH and known native installation directories.
Codex also supports nested and hoisted npm platform packages. Managed JavaScript
adapters run through Node; installation requires `node.exe` beside
`node_modules/npm/bin/npm-cli.js`. General batch wrappers, older Codex vendor
layouts, and Volta's selected tool-image npm location are unsupported.

An OS file lock prevents engines from sharing a profile; `engine.lock.pid` is
only diagnostic. Terminals use ConPTY. Windows agents, login commands, adapter
installs, and terminals own their child process trees through Job Objects.
Terminal close waits up to five seconds for cleanup and reports failure;
shutdown/drop log failures. Processes started through external services or
brokers are outside this ownership.

Frosted window chrome uses native Acrylic and requires Windows **Settings >
Personalization > Colors > Transparency effects**. Content cards and popovers
remain opaque because in-app backdrop blur is not supported. The pinned
[Zui DirectX fix](https://github.com/zeronsh/zui/pull/7) supplies the renderer
layout and edge-fade corrections.

## Verification

[Windows CI](../../.github/workflows/windows.yml) builds the release application
and tests application startup, agent discovery/protocols, process cleanup,
ConPTY, locking, UI behavior, and shader layouts. Shared Rust regressions run
on Linux and macOS. To run the engine and harness checks locally:

```powershell
cargo test --locked -p zeron-engine -p zeron-harness --lib
cargo test --locked -p zeron-harness --features native-fixture --test codex_availability --test windows_native
cargo test --locked -p zeron-engine --test codex_catalog
```

Fixtures use synthetic agents, so these tests do not establish authenticated
provider compatibility. GUI probes are optional CI dispatch checks and can
also run on an interactive Windows desktop after building the release app:

```powershell
cargo build --release --locked -p zeron-ui --example windows-render-fixture --features windows-render-fixture
./scripts/test-windows-lifecycle.ps1 -Runs 5
./scripts/test-windows-rendering.ps1
```

The lifecycle probe uses isolated data and provider homes. The renderer probe
captures only its synthetic window. Results go to ignored
`target/windows-lifecycle-*` and `target/windows-render-*` directories.

Remaining acceptance work includes authenticated provider runs and shutdown,
broader GPU/DPI coverage, accessibility, native browser and cross-device parity,
concurrent-launch log rotation, and invalid-window teardown diagnostics.
