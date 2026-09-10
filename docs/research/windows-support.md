# Windows support research

Researched 2026-09-10. This report distinguishes the project's pinned dependencies from current upstream documentation and proposed implementation work. Online support claims do not establish that Zeron builds or that its harnesses work on Windows.

**Recommendation:** retain the Rust/GPUI architecture and target a native Windows 11 x64 preview first. The unmodified application passes `cargo check --locked -p zeron` on Windows/MSVC. Shipping support still requires runtime and renderer fixes, portable test coverage, and Windows packaging. Start with a rendering/startup proof and one native agent; use the existing external-browser fallback until WebView2 is validated. WSL can be a separate optional engine environment.

## Feasibility from primary sources

**A native Windows port can retain GPUI.** The exact `zeronsh/zui` revision in the root Cargo manifest, `07fd941ad72e7edc812fed317aab66adb69fa8cc`, already selects `gpui_windows::WindowsPlatform` on Windows. Its Windows crate has native Windows and accessibility dependencies. This is evidence about the actual dependency pin, not an assumption based on newer Zed versions. [Pinned platform selection](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui_platform/src/gpui_platform.rs), [pinned Windows manifest](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui_windows/Cargo.toml).

The pinned Windows backend compiles HLSL shaders for release builds and locates `fxc.exe` through `GPUI_FXC_PATH`, PATH, or Windows SDK registry discovery. A successful debug build alone will therefore not validate the release toolchain. [Pinned Windows build script](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui_windows/build.rs).

**The custom fork needs a rendering compatibility audit.** In the fetched pinned source, Rust `Quad` includes `fade`, and `PolychromeSprite` places `fade` before `tile`; the Windows HLSL structures omit these fields. The latter changes the expected offset of atlas data, so this is a potential rendering correctness problem rather than merely a missing visual effect. The Windows renderer also does not implement the fork's backdrop-blur path. Verify CPU/GPU layouts, port required effects or supply deliberate fallbacks, and run actual rendering checks before treating the existing backend as production-ready. [Pinned scene structures](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui/src/scene.rs), [pinned Windows shaders](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui_windows/src/shaders.hlsl), [pinned DirectX renderer](https://raw.githubusercontent.com/zeronsh/zui/07fd941ad72e7edc812fed317aab66adb69fa8cc/crates/gpui_windows/src/directx_renderer.rs).

Current upstream Zed officially distributes Windows builds and documents a DirectX 11 compatible GPU requirement. Its build instructions call for Rust, MSVC C++ tools, Windows SDK, and CMake. These are useful starting points, not proof that every Zed prerequisite applies to this smaller extracted dependency. [Zed Windows support](https://zed.dev/docs/windows), [Zed Windows build instructions](https://zed.dev/docs/development/windows).

**The terminal dependency already has a Windows route.** `portable-pty` 0.8.1 documents a cross-platform PTY abstraction; the locally cached 0.8.1 source confirms `NativePtySystem = win::conpty::ConPtySystem` under `cfg(windows)`. Upstream source has the same selection. Windows ConPTY is available starting with Windows 10 version 1809; this API floor is not a proposed Zeron support policy. Retain the abstraction and test actual shell selection, resize, Unicode, cancellation, EOF, and process cleanup. [portable-pty 0.8.1 docs](https://docs.rs/portable-pty/0.8.1/portable_pty/), [upstream PTY source](https://raw.githubusercontent.com/wez/wezterm/main/pty/src/lib.rs), [Microsoft CreatePseudoConsole requirements](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole).

## Embedded browser implementation options

Wry's documented `WebViewBuilder::build_as_child` can host a webview inside a Windows parent window through `HasWindowHandle`; bounds can be supplied for a subregion. This makes a Wry/WebView2 implementation a plausible continuation of the existing browser abstraction. It is a proposal until tested with Zeron's pinned GPUI window, clipping, focus, dialogs, and overlays. [Wry child-webview API](https://docs.rs/wry/latest/wry/struct.WebViewBuilder.html#method.build_as_child).

WebView2 must be created and called on an STA UI thread with a message pump. Do not move its COM objects into engine background tasks or synchronously block its UI callbacks while awaiting script results. [Microsoft WebView2 threading model](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/threading-model).

A shipping browser feature requires the WebView2 Runtime, even when Edge is installed. Windows 11 includes Evergreen, but Microsoft still recommends detecting runtime availability. An installer can deploy the Evergreen bootstrapper when missing; Fixed Version offers control over updates at the cost of bundling the runtime. Recommended initial approach: Evergreen detection and a clear installation path. [Microsoft WebView2 distribution guidance](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution).

A browser spike should prove navigation, page title/loading events, JavaScript evaluation, screenshots if used by the app, popup handling, keyboard focus traversal, per-monitor DPI, and visibility when tabs or panels are hidden. In particular, test GPUI menus and modals overlapping the native child window before claiming feature parity. If that integration delays the first desktop milestone, the existing external-browser fallback in `crates/ui/src/browser/view.rs` can support a deliberately scoped interim feature.

## Agent availability and integration limits

| Agent/protocol | What official sources establish today | Implication for Zeron |
| --- | --- | --- |
| Claude Code | Native Windows and WSL are supported. Current docs make Git for Windows optional: it provides Bash; without it, Claude uses PowerShell. Native Windows sandboxing is unavailable in the documented support table, while WSL2 supports it. | Native integration is feasible; test the installed CLI version, stream protocol, cancellation, shell environment, and capability differences. Do not repeat outdated claims that Git Bash is always mandatory. |
| Codex | Official OpenAI documentation supports using Codex on Windows through the CLI and describes a native Windows sandbox. | Native support is a viable target. Installed app-server protocol and sandbox configuration still need end-to-end validation with this repository's adapter. |
| Cursor CLI | Current installation docs include both native Windows PowerShell installation and Windows through WSL. | Do not label it WSL-only. Validate the exact executable name, installed version, adapter protocol, and lifecycle before advertising support. |
| ACP agents | ACP defines UTF-8 JSON-RPC over subprocess stdin/stdout, with newline-delimited messages. | The protocol does not inherently require Unix. Availability depends on each agent's binaries, runtime, executable discovery, environment, and launch behavior. |

Sources: [Claude Code system requirements and Windows setup](https://code.claude.com/docs/en/setup#set-up-on-windows), [official OpenAI Windows documentation](https://learn.chatgpt.com/docs/windows/windows-sandbox), [Cursor CLI installation](https://cursor.com/docs/cli/installation), [ACP transport specification](https://agentclientprotocol.com/protocol/v1/transports).

These vendor pages change over time. Support should be recorded as a tested agent/version/platform matrix, rather than inferred forever from an installation page. A CLI running interactively is weaker evidence than Zeron successfully creating, resuming, interrupting, and terminating an agent session.

## Proposed support scope

Start with native Windows 11 x64, MSVC, the existing GPUI desktop and local engine, and one verified native agent. This is a deliberate initial test matrix, not a claim that Windows 10 or ARM64 is impossible. Expand only after a working release build and lifecycle tests.

Treat WSL as a separate execution environment. If offered later, run the whole engine and its agents inside the selected distribution so repository paths, git, worktrees, shells, and credentials remain in one environment. A native desktop talking to that engine is an architectural proposal; automatic discovery, launching, transport, path presentation, and authentication require explicit design. Merely wrapping individual native-engine commands in `wsl.exe` is not a complete WSL integration.

## Evidence still required

- Link and launch Zeron itself on Windows using the pinned dependencies, including release shader compilation. The debug Cargo check passed; it does not establish these gates.
- Validate desktop rendering, fonts, clipboard, shortcuts, file dialogs, DPI changes, and accessibility on a real Windows desktop.
- Validate native agent launch and lifecycle, terminal subprocess cleanup, persistence/recovery, git operations, worktree paths, and sync across Windows and Unix devices.
- Validate the updater/installer on an existing installation, since installing a fresh executable does not exercise replacement of a running Windows program.
- Complete the embedded-browser spike before promising parity with macOS.

## Repository audit at c31b440 (0.2.54)

These findings are from source inspection, not successful runtime tests. Relative links refer to this repository at the reviewed revision.

| Area | Existing behavior and gap | Proposed action |
| --- | --- | --- |
| Application data | `dirs_data_dir()` requires `HOME` and panics when absent. Other components already fall back to `USERPROFILE`, so resolution is inconsistent. [Entry point](../../apps/zeron/src/main.rs), [repository paths](../../crates/engine/src/repos.rs) | Centralize user/data/cache path resolution, preserve `ZERON_DATA_DIR`, and choose/document a Windows default such as `%LOCALAPPDATA%/Zeron`. Keep provider credential locations separate from Zeron storage. Test Explorer launch without `HOME`. |
| Engine ownership | `InstanceLock::acquire` only locks under `cfg(unix)`; Windows still opens/stamps the file and returns success. `holder` returns `None` on non-Unix, weakening login/logout guards too. [Instance lock](../../crates/engine/src/instance_lock.rs) | Implement and test a real Windows lock plus a non-destructive holder probe before permitting local sessions. Include account credential synchronization and log rotation in the locking audit. |
| Agent discovery | Claude and Codex explicitly search for `.exe`, but their fallback directories still assume Unix layouts. ACP's `find_on_paths` checks the exact supplied filename; calls for `node` and `npm` can miss Windows executables or select an extensionless shell shim. [Claude](../../crates/harness/src/claude/mod.rs), [Codex](../../crates/harness/src/codex/mod.rs), [ACP](../../crates/harness/src/acp/mod.rs) | Share a Windows-aware resolver that distinguishes native executables, JS entries, and command shims. Preserve explicit overrides and test native installs and npm installs separately. |
| Managed adapters | Adapter storage requires `HOME`. Native executable detection recognizes ELF/Mach-O, not Windows PE; other entries are launched through Node. [Installer](../../crates/harness/src/adapter_install.rs) | Resolve Windows directories, recognize PE binaries, and prefer `node.exe` with the actual JS entry where applicable. Handle npm launch explicitly. |
| Process lifetime | Non-Unix `send_signal` is a no-op. `shutdown_child` eventually calls `start_kill`, but this does not establish cancellation correctness for every harness timer or descendant process. [Lifecycle helpers](../../crates/harness/src/lib.rs) | Retain protocol-level cancellation first, then bounded termination through a Windows process supervisor. Test unresponsive children and grandchildren; prevent background helper console flashes. |
| Terminal | Already selects `COMSPEC` (normally cmd.exe), otherwise PowerShell, and omits Unix `-l`; uses portable-pty. [Terminals](../../crates/engine/src/terminals.rs) | Validate ConPTY rather than replace it. Add explicit shell preference/discovery if desired, with cmd and PowerShell fixtures. |
| Files and repositories | Home lookup supports `USERPROFILE`, drive-letter enumeration exists, and editor replacement has a Windows `MoveFileExW` branch. Workspace RPC paths intentionally use relative slash-separated paths and reject backslashes/colons. [Repositories](../../crates/engine/src/repos.rs), [workspace files](../../crates/engine/src/workspace_files.rs) | Preserve wire paths and convert only at the owning engine. Test drive/UNC/canonical path forms, case, junction containment, reserved names, long and Unicode paths, CRLF, locked files, and Git for Windows worktrees. Do not globally rewrite all path strings. |
| Credentials | WorkOS private writes enforce Unix permissions only; non-Unix uses a plain write. Account integration also has provider-specific assumptions. [Auth](../../crates/engine/src/auth.rs), [agent accounts](../../crates/engine/src/agent_accounts.rs) | Validate user-directory ACLs, custom-directory behavior, atomic replacement, and provider-native login/storage. Scope the initial release to verified provider flows; avoid a global credential-directory migration. |
| Browser | Native implementations exist only for macOS/Linux. An external-browser fallback is already implemented for other platforms. [Browser](../../crates/ui/src/browser/mod.rs), [browser view](../../crates/ui/src/browser/view.rs) | Use that fallback for an explicitly scoped preview, then implement and validate a Windows WebView2 surface. |
| Notifications and sound | Windows banners are currently a no-op. A Windows sound path already invokes PowerShell; its interpolated file path needs quoting review for apostrophes in user/temp directories. [Notifications](../../crates/ui/src/notify.rs), [sound](../../crates/ui/src/sound.rs) | Add Windows application identity/toasts with packaging; test sound paths and suppress helper windows. |
| Daemon lifecycle | CLI service operations implement launchd/systemd. Engine shutdown on non-Unix listens only for Ctrl-C. [Daemon CLI](../../apps/zeron/src/daemon.rs), [engine shutdown](../../crates/engine/src/lib.rs) | First validate embedded/foreground mode. Then add per-user background startup and a graceful stop mechanism that flushes documents; do not assume task termination delivers Ctrl-C. |
| Updating | `platform_key()` calls every non-macOS platform `linux`; managed installation uses a Unix symlink switch and existing service restart paths. [Updater](../../crates/update/src/lib.rs) | Correct platform/artifact selection before exposing updates. Use a Windows installer or separate updater that waits for exit, replaces files, and can recover; do not reuse Linux artifact selection. |
| Tests and distribution | Release jobs package Linux/macOS only. Tests include shell fixtures and some Unix-only imports under plain `cfg(test)`, for example source-control tests. [Release workflow](../../.github/workflows/release.yml), [UI CI](../../.github/workflows/ui-tests.yml), [source-control tests](../../crates/engine/src/source_control.rs) | Add Windows CI and portable fixture executables. Gate only truly Unix-specific tests, replacing lost behavior coverage with Windows tests. Add Windows packaging and release-manifest gating. |

### Implementation choices supported by platform documentation

- **Locks:** Rust's `File::try_lock` maps to Windows `LockFileEx` and provides nonblocking exclusive locking. It is a candidate for a shared implementation, with release/probe behavior tested on Windows; do not mix lock APIs casually on the same handle. [Rust file locking](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).
- **Process trees:** Windows Job Objects can group descendants and terminate associated processes when the final job handle closes with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Use an intentional per-run/process-group design; account for nested jobs and lifecycle ownership rather than indiscriminately killing every child of the desktop. [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).
- **Launch arguments:** Rust can resolve omitted `.exe` extensions, but that does not fix a custom resolver rejecting the file before spawn. Non-`.exe` extensions and command-script quoting need explicit handling. Preserve separate arguments and avoid building a `cmd /c` string containing prompts or workspace paths. [Rust Command](https://doc.rust-lang.org/std/process/struct.Command.html).
- **Background startup:** A task running under the signed-in user's interactive token is a possible initial daemon design, preserving the user's environment and credentials. It means startup at logon, not a guarantee of unattended operation before login; a true Windows Service needs a separate account/session design. [Task security contexts](https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks).
- **Filesystem tests:** Windows has reserved names and distinct drive, UNC, and extended-path namespaces. Add Windows cases to the existing containment policy rather than assuming Unix tests cover them. [Microsoft naming rules](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file).

## Delivery plan and acceptance gates

Recommended order; these are proposed work packages, not completed implementation or calendar estimates.

1. **Prove the pinned stack.** Add Windows x64/MSVC build coverage, resolve compile failures, and audit/fix the zui CPU/HLSL layout mismatch in the fork before updating its pin here. Verify a release build with `fxc.exe`, not just `cargo check`. Launch a rendering fixture showing image atlas sprites, text, scroll fades, and popovers at multiple DPIs. An opaque fallback may defer backdrop blur, but it does not fix mismatched GPU fields.
2. **Make local sessions reliable.** Fix startup directories and ownership locks, centralize process/executable handling, and verify one native agent. Gate on clean startup without `HOME`, restart/resume, concurrent launch rejection, interrupt of a wedged process tree, attachments, file editing, git/worktrees, and terminal resize/replay. Keep external-browser fallback explicit.
3. **Verify the multi-device product.** Test Windows-hosted sessions from macOS/iOS and Windows viewing Unix-hosted sessions, including offline commands, reconnect, attachments and remote files. Audit UI code that assumes the engine's paths belong to the desktop OS. The current edge protocol can be retained in principle; compatibility testing must confirm that platform paths do not leak into wire assumptions.
4. **Make installation and background operation supportable.** Produce a Windows preview package, then installer/application identity, optional startup, graceful daemon stop, versioned artifacts, and an update/rollback path tested with an existing running installation. Keep Windows out of advertised release support until its artifact joins the required release jobs. Preserve useful CLI output while preventing unwanted console windows in desktop/helper launches.
5. **Complete platform parity.** Implement WebView2, notifications, additional native agents and accessibility/polish. Add ARM64 or older Windows only after checking the full dependency, installer, and provider matrix. Do not equate the ConPTY API minimum with the application's supported OS minimum.

For faster feedback, a UI-free build feature or separate headless binary is worth considering: `apps/zeron` currently depends on `zeron-ui` unconditionally, so the `headless` subcommand still compiles GPUI. This is optional build isolation, not a prerequisite for the product architecture. [Binary manifest](../../apps/zeron/Cargo.toml).

### Native versus WSL

| Route | Benefit | Remaining work |
| --- | --- | --- |
| Native desktop + native engine | Fits existing connect-or-embed behavior and Windows repositories | All native lifecycle, filesystem, rendering and packaging gates above |
| Native desktop + entire engine in WSL2 | Retains Linux agent/worktree behavior | Still needs the Windows UI; explicit distribution selection, engine lifecycle, endpoint discovery and path handling. Current connect-or-embed can silently create a native engine when a WSL engine is absent, so WSL selection must change that fallback behavior. |
| Linux desktop app through WSLg | Potential interim route without a native UI build | A Linux installation/workflow, not native Windows support; not tested here and dependent on Linux GPU/browser/runtime setup |

Microsoft documents Windows access to WSL-hosted network services through localhost. That makes the existing RPC boundary promising for WSL integration, but it does not validate app discovery, VPN/firewall behavior, multiple distributions, or restart recovery. Preserve Linux paths and credentials inside the Linux engine rather than sharing its SQLite directory with a Windows engine. [WSL networking](https://learn.microsoft.com/en-us/windows/wsl/networking), [existing engine attachment](../../crates/ui/src/state.rs).

### Suggested validation matrix

- Debug check **and release build** on `x86_64-pc-windows-msvc`; native rendering test on a real desktop, including image sprites and DPI changes.
- Local-only startup, saved synced profile, login/logout while an engine owns the directory, second-process contention, normal shutdown and crash recovery.
- Native executable and npm-based agent installs; paths with spaces/apostrophes/non-ASCII; absent `HOME`; missing runtime; cancel, resume, tool questions, and child/grandchild cleanup.
- ConPTY with cmd and PowerShell: Unicode, large output, resize, Ctrl-C, EOF, disconnect/reconnect and close.
- Git repository and projectless sessions; worktree creation; CRLF; files held open; junctions; drive and UNC paths; account-scoped attachments; Windows/Unix remote file interoperability.
- Offline queue and reconnect across Windows and another device; installation, update while running, failed update recovery, uninstall that preserves workspace data.

## Local build probes

Host: `x86_64-pc-windows-msvc`; Rust 1.97.0, Cargo 1.97.0. Source: `c31b440`, workspace version 0.2.54, unchanged application code and lockfile.

`cargo check --locked -p zeron` **passed**, exit 0, in 4m 11s after fetching dependencies. This checked the application, engine, UI and pinned Windows GPUI dependency. Existing unused-variable/dead-code warnings and a dependency future-compatibility warning were emitted. It did not link an executable, compile release shaders, open a window, run an agent, or run tests.

`cargo check --locked -p zeron-engine --tests --message-format short` **failed**, exit 1. The observed errors were `E0433` at `crates/engine/src/source_control.rs:855` for `std::os::unix::fs::PermissionsExt`, and `E0599` at line 1057 for `Permissions::set_mode`. These are test-code portability failures, consistent with the audit above. This check does not run tests or prove that these are the only failures in the wider workspace test suite.

No application implementation changes were made as part of this research. The report is the only intended tracked addition; dependency caches and ignored build output were populated by the checks.
