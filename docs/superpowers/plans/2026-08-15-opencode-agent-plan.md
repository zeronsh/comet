# OpenCode agent over ACP — implementation plan

Date: 2026-08-15
Design: docs/superpowers/specs/2026-08-15-opencode-agent-design.md (approved)

Implementation order (user decision): **harness + proto first, then UI + docs**,
one verify pass at the end.

## Steps

### Step 1 — proto: `HarnessId::OpenCode`
`crates/proto/src/agent.rs`: add `OpenCode` variant with doc comment. kebab-case
serialization gives `"opencode"`. No new `ReasoningLevel`.

### Step 2 — harness spec + constructor
`crates/harness/src/acp/mod.rs`:
- `fn opencode_spec() -> AcpAgentSpec` (mirror `cursor_spec()`/`hermes_spec()`):
  id OpenCode, display_name "OpenCode", executable `opencode`, args `["acp"]`,
  `env_override: Some("OPENCODE_EXECUTABLE")`, `npx_package: None`,
  extra_paths = `~/.opencode/bin` + npm/homebrew dirs + `node_version_manager_bins()`,
  TurnBoundary steering, empty reasoning levels, identity prompt transform /
  default effort, `models: || Vec::new()` (wire-first).
- `pub fn opencode() -> Self` constructor + doc comment.
- Wire the new spec into any harness-side exhaustive matches on `HarnessId`
  if present (compile will catch).

### Step 3 — harness tests
`crates/harness/tests/acp.rs`, per-agent test pattern:
- installed() probe (PATH, `~/.opencode/bin`, `OPENCODE_EXECUTABLE` override).
- launch_program() → `opencode` `["acp"]`.
- models_from_session with mocked configOptions: Zen models + `mode`
  build/plan as ModelOption, empty ladder.
- steering_mode TurnBoundary; commands via available_commands_update.
- `OPENCODE_EXECUTABLE` fixture → fake binary; no real opencode needed.

### Step 4 — engine registry
`crates/engine/src/registry.rs`: lazy slot for OpenCode (mirror of spec, opt-in,
`enabled: None`). Update registry tests asserting full slot list / resolve /
enable coverage to include `HarnessId::OpenCode`.

### Step 5 — UI
- `crates/ui/src/settings/harnesses.rs`: `blurb()` + `cli_name()` arms.
- `crates/ui/src/pickers.rs`: `harness_brand_icon()` arm → `OPENCODE_MARK`.
- `crates/ui/src/icons.rs`: register `OPENCODE_MARK` in `icon_assets!`.
- `crates/ui/assets/icons/opencode-mark.svg`: new monochrome asset.
- No change to accounts.rs (fallback `_` arm).

### Step 6 — docs
- `README.md` l. 3: add OpenCode to agent list.
- `crates/harness/src/lib.rs` header: mention opencode native ACP.

### Step 7 — verify
- `cargo build` (workspace).
- `cargo test -p zeron-harness -p zeron-engine` (plus any crate the new code
  touches). Check for a lint/typecheck command (clippy) and run it.

## Verification commands (repo conventions — confirm names before running)
- Build: `cargo build` (workspace).
- Tests: `cargo test -p zeron-proto -p zeron-harness -p zeron-engine -p zeron-ui`.
- Lint: `cargo clippy --workspace` if configured.