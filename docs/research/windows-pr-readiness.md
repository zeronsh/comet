# Windows support review guide

This branch adds native Windows source-build development. Windows releases,
installers, managed updates, and background services are outside this change.
See [Windows development](../reference/windows-development.md) for setup and
commands, and [verification history](windows-verification-history.md) for dated
local evidence.

## Dependency changes

The renderer fix has landed in [zui#7](https://github.com/zeronsh/zui/pull/7).
[gpui-component#4](https://github.com/zeronsh/gpui-component/pull/4) aligns its
GPUI dependencies with that fix. Comet consumes both merged upstream revisions:

| Dependency | Revision | Purpose |
| --- | --- | --- |
| `gpui`, `gpui_platform`, `gpui_tokio` | `3151ad1` | Windows CPU/HLSL layouts and quad/image edge fades. |
| `gpui-base` | `3a8c458` | Uses the same Zui revision as Comet. |

These pins and `Cargo.lock` must move together: different Git revisions create
separate GPUI types. No fork URL, local Cargo override, or vendored renderer is
needed. The renderer implementation and layout regressions are reviewed upstream.

## Suggested reading order

| Area | Entry points | Behavior to review |
| --- | --- | --- |
| Application startup | `apps/zeron/src/paths.rs`, `crates/engine/src/instance_lock.rs` | Native application-data roots and OS file-lock ownership. PID sidecars are diagnostic only. |
| Agent discovery and launch | `crates/harness/src/executable.rs`, `crates/harness/src/adapter_install.rs` | Shared native discovery, Codex npm payload resolution, and direct Node/native launches with argument arrays. |
| Terminal lifecycle | `crates/engine/src/terminals.rs` | Observe child exit independently of EOF, release ConPTY handles, and drain buffered output before reporting exit. |
| UI integration | `crates/ui/src/state.rs`, `crates/ui/src/workspace_links.rs`, `crates/ui/src/theme.rs`, `crates/ui/src/shell.rs` | Await cancelled bootstrap IPC, recognize remote POSIX roots, use native Acrylic with opaque content surfaces, and make tab close clicks independent of tab dragging. |
| Platform boundaries | `apps/zeron/src/daemon.rs`, `apps/zeron/src/update_cli.rs`, `crates/update/src/lib.rs` | Keep Windows source installations unmanaged and guard unsupported service/update operations. |
| Regression coverage | `.github/workflows/windows.yml`, `crates/harness/tests/windows_native.rs`, `crates/engine/tests/codex_catalog.rs`, `scripts/test-windows-*.ps1` | Build/link, synthetic agent discovery/protocol, ownership, ConPTY, and optional native rendering/lifecycle probes. |

Shared terminal, discovery, and bootstrap changes also affect Unix code paths.
The workflow includes Linux/macOS harness, engine, and updater library tests.
Native GUI probes require manual dispatch with a usable desktop; they are not
ordinary PR-time GPU coverage.

## Evidence and limits

Report fresh checks against the final PR revision in the PR description.
The [verification archive](windows-verification-history.md) predates the final
upstream dependency transition and must not be read as a passing result for it.
CPU/HLSL layout tests check offsets and source declarations; release linking
compiles shaders, while native fixture captures check actual rendered pixels.

Remaining acceptance work includes authenticated provider sessions, descendant
process-tree cleanup, multiple GPUs/DPIs, accessibility/native browser parity,
and Windows packaging/update/rollback. In-app backdrop blur is unsupported;
window Acrylic depends on the Windows transparency preference.
