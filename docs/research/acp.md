# ACP integration: shared harness + Grok Build (2026-08)

## Decision
- Add an **ACP harness** (`crates/harness/src/acp/`) speaking Agent Client Protocol
  v1 — JSON-RPC 2.0 newline-framed over stdio, same wire shape as the codex
  app-server — over the shared `crates/harness/src/jsonrpc.rs` client (promoted
  from `codex/rpc.rs`). Wire types are hand-rolled tolerant serde against raw
  `Value`s (house style, verified against `agent-client-protocol-schema` 1.3.0),
  NOT the official SDK crates: comet keeps its own child-lifecycle hardening
  (StderrTail, SIGTERM→SIGKILL, PATH composition) and shell-script test
  fixtures, and drives raw updates the SDK's `ActiveSession` abstraction hides.
- First registered agent: **Grok Build** (`grok agent stdio`), xAI's native ACP
  agent (npm `@xai-official/grok`, ACP registry id `grok-build`). Auth: browser
  OAuth or `XAI_API_KEY`; comet passes env through. `GROK_EXECUTABLE` overrides
  resolution (tests point it at `tests/fixtures/fake-acp.sh`).
- **claude/codex converted to ACP too** (2026-08-08, wing's call: "keep things
  clean"): `AcpHarness::claude()` via `@agentclientprotocol/claude-agent-acp`
  (pinned 0.66.0) and `AcpHarness::codex()` via `@agentclientprotocol/codex-acp`
  (pinned 1.1.14), resolved from PATH or launched through `npx -y <pinned>`.
  The bespoke stream-json/app-server adapters (~4,300 lines) are deleted; the
  catalogs (models, effort clamping, Ultrathink prefix) survive as spec inputs.
  Accepted deltas: Claude steering is now priority-`now` pre-emption (adapter
  semantics) instead of step-boundary stdin; sandbox policy control is
  adapter-owned; comet-specific settings ride config options where advertised
  (mode → bypassPermissions, model via family-alias matching — the claude
  adapter advertises SDK aliases like `opus[1m]`/`sonnet`/`haiku` —
  fastMode/thinking as booleans) and are silently skipped elsewhere
  (ultracode has no adapter surface today). AskUserQuestion arrives as a
  question-shaped `session/request_permission` (options without allow/reject
  kinds) and bridges to the input panel; allow/reject-shaped requests
  auto-accept. Per-turn usage comes from the settled prompt response.

- **Hermes + Pi registered** (2026-08-08): `AcpHarness::hermes()` runs Nous
  Research's native ACP server (`hermes acp`; Python install via
  `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash` plus the
  `.[acp]` extra — no npm fallback, so resolution is PATH/`~/.local/bin`/
  `~/.hermes/bin` only, `HERMES_EXECUTABLE` overrides). No
  `_session/steering` extension and no effort config advertised (Hermes 4's
  hybrid reasoning is model-internal) → turn-boundary steering, empty ladder;
  static catalog lists the Nous portal flagships (Hermes 4 405B/70B), real
  model set derives from the user's authenticated providers and unknown ids
  skip through the config-option set. `AcpHarness::pi()` runs the pi coding
  agent (pi.dev) through the community `pi-acp` adapter (pinned 0.0.33,
  `npx -y` fallback; requires the pi CLI itself,
  `@earendil-works/pi-coding-agent`; `PI_ACP_EXECUTABLE` overrides). Models
  ride pi's own provider config (catalog advertises a `default` pass-through
  entry); thinking ladder minimal→max maps onto comet's levels via the
  generic `thought_level` preference ladder ("off" has no comet tier).

## Protocol surface used (v1)
- `initialize` (protocolVersion 1; fs/terminal client capabilities declined) →
  `session/new` / `session/load` (fresh-session fallback; replay drained, the
  doc already holds history) → `session/prompt` per turn; the prompt RESPONSE
  carries the `stopReason` (`cancelled` → Interrupted, `refusal` → Errored,
  else Completed). `session/cancel` is the interrupt; SIGTERM/SIGKILL escalate.
- `session/update` notifications → `AgentEvent`: message/thought chunks →
  Text/ReasoningDelta; `tool_call`/`tool_call_update` → typed ToolCall (kind +
  rawInput + locations + diff content) + ToolResult carrying **capped output
  text and inline diffs** (16KB/64KB harness caps; 4KB/16KB doc caps in
  `parts.rs` — the session-load-size discipline); `plan` → `ToolCall::Todo`
  (stable id `acp-plan`); `available_commands_update` →
  `AgentEvent::AvailableCommands`. `usage_update` is a context gauge, not
  per-turn tokens — deliberately unmapped.
- `session/request_permission` → auto-accept the preferred allow option
  (`allow_always` > `allow_once` > first) — parity with claude
  bypassPermissions / codex approvalPolicy never.
- **Session config options**: ACP has no per-prompt model field; the run's
  model + reasoning apply through `session/set_config_option` against the
  session response's advertised `configOptions` (category `model` /
  `thought_level`, matched to advertised value ids, skipped when current,
  never fatal). Grok's effort ladder in the picker is Low/Medium/High →
  `low`/`medium`/`high`; other comet levels degrade down a preference ladder
  (`config_option_sets`).
- Steering: `_session/steering` extension when
  `initialize._meta.steering.supported` (org adapters); request carries
  `_meta.steering.idleBehavior: "promptRequired"` so a turn-end race hands the
  text back instead of firing an untracked turn. Without the extension (Grok):
  queue and deliver as the next `session/prompt` — `SteeringMode::TurnBoundary`.
  Session parks between turns while the steering mailbox lives (codex pattern).
- Ordering hazard fixed twice: responses resolve via the pending map while
  notifications ride the incoming channel, so (a) `request_draining` flushes
  the channel after `session/load` resolves, (b) the turn arm drains queued
  updates before emitting Done, (c) EOF right after a final response reads as
  a clean finish (50ms turn-future grace), not a crash.

## New shared surface
- `Harness::commands()` (default empty) + `ListCommands` RPC (mirrors
  ListModels, relay-forwardable) → composer `/` popup (mirrors the file-mention
  popup; local `filter_indices` ranking, no per-keystroke RPC).
- `AgentEvent::ToolResult{output?, diff?}` → `MessagePart::Tool{output?, diff?}`
  → doc columns (`output`, `diff` — additive, TS mirror updated in
  render-parts.ts/control-types.ts) → expandable transcript chips
  (`tool_detail_lines`: `similar` line diff, context collapsed to `⋯`, 12-line
  cap, analytic heights).

## Citations
agentclientprotocol.com (v1 spec + schema), agentclientprotocol org repos
(claude-agent-acp v0.66.0, codex-acp v1.1.14 — steering wire shape),
agent-client-protocol-schema 1.3.0 (serde tags), ACP registry entry
`grok-build`, live `grok agent stdio` initialize handshake (2026-08-07).
