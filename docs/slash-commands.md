# Slash commands: per-workspace discovery

Status: IMPLEMENTED on `slash-commands-per-workspace`, not yet merged · 2026-08-16 investigation (project skills missing from the composer popup) · merged with `main` on 2026-08-19, after #160 gave the native claude/codex drivers their own per-harness discovery.

Every `file.rs:NNN` reference below is a snapshot of the 2026-08-16 investigation. The
reasoning holds; the line numbers have moved.

## Why

Project skills never appear in the slash-command popup. A repo with skills in
`<repo>/.claude/skills` shows only the built-in and user-level commands.

The cause is one line. Command discovery asks the agent for a session in the wrong
directory (`crates/harness/src/acp/mod.rs:782-784`):

```rust
let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
let session = client
    .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
```

ACP agents build the command list from the session `cwd`. Project skills live under the
project. With `cwd = $HOME` the agent never sees them.

### Measured

The `claude-agent-acp` 0.66.0 adapter was driven twice by hand. Only the `session/new`
cwd changed:

| session cwd | commands |
|---|---|
| `~/Documents/AppDev/read-aloong` | 110 |
| `$HOME` | 76 |

The 34 missing entries are exactly that project's installed skills: `ask-matt`, `tdd`,
`wayfinder`, `wizard`, `triage`, `prototype`, `research`, `grill-with-docs`, and the rest.

The skills were installed as symlinks into a content-addressed store. The agent followed
every symlink once the cwd was right. Symlinked skill directories are not a factor.

### The real shape of the bug

ACP advertises commands **per session**, and a session is defined by its `cwd`. Zeron
models commands **per harness**, with no workspace anywhere in the path. Three layers
carry that mismatch, and fixing one alone changes nothing:

1. **Discovery cwd.** `discover_commands` spawns with `spawn_agent(None, ...)` and sends
   `cwd = $HOME` (`acp/mod.rs:767-784`).
2. **RPC shape.** `ListCommands` borrows `ListModelsParams`, which carries only `harness`
   (`engine/src/rpc.rs:85-87`, `1023-1036`). There is no field for a workspace.
3. **Two caches keyed by harness only.** The `OnceCell` in the harness (`acp/mod.rs:548`)
   and the composer's `slash_cache` (`ui/src/composer.rs:3312`, `4018`). With the cwd
   threaded through but the keys unchanged, the first project's list would serve every
   project.

A fourth fact shapes the design, and it is worse than it first looks. A live session runs in
the real cwd, so it holds the correct list. Zeron never sees it. Two separate reasons:

1. **The existing emission never fires for Claude.** `acp/mod.rs:2218-2222` emits
   `AgentEvent::AvailableCommands`, but only from `init_commands`, which
   `scan_available_commands` reads out of the **initialize** response
   (`acp/mod.rs:2004`). Initialize runs before `session/new`, so that list is not
   cwd-scoped, and for `claude-agent-acp` it is empty. Measured: driving the adapter by
   hand, initialize and the `session/new` response both carry no commands. All 110 arrive
   in one `available_commands_update` notification after `session/new`.
2. **That notification is dropped.** Every run sends `session/new` through
   `request_draining` (`acp/mod.rs:2036`, and `2010`/`2018` for the resume paths). That
   helper answers server requests and discards notifications (`Some(_) => {}`,
   `acp/mod.rs:1895`); its post-response flush handles only `Incoming::Request`
   (`acp/mod.rs:1907-1911`). The update lands inside exactly that window.

Only a mid-session update, sent after the handshake, reaches the main loop and
`normalize.rs:365-368`.

Then `doc/src/parts.rs:339-343` drops whatever does get through, and no engine code reads
it. The comment there claims the event "feeds the engine's per-harness command cache". No
such cache exists. The comment is stale.

## Design

The identity of a command list becomes `(harness, cwd)`.

The `cwd` is a path on the **host device** that owns the agent, so a space on another
device uses that device's path. `~` travels unexpanded and expands on the host, matching
how run cwd already works (`composer.rs:4536-4539`).

### Topology

```
                        probe (cold, TTL-bounded)
composer popup ── ListCommands{harness, cwd} ── engine CommandCache ── Harness::commands(cwd)
     ^                                               ^
     └── stale-while-revalidate render               └── AgentEvent::AvailableCommands
                                                         (live, from a running session)
```

Two sources feed one cache. The probe serves cold projects and chats that never started.
The live event corrects any chat that is running.

### Making the live event real

The live leg does not work today, for the two reasons in Why. It needs one contained change
in the harness, in the run path:

- `request_draining` gains an out-parameter for `available_commands_update`. It keeps
  discarding every other notification, which is the behavior its doc comment describes and
  the reason it exists (a replayed `session/load` must not re-enter the doc).
- After the handshake, the run emits `AgentEvent::AvailableCommands` from the captured
  update when there is one, and from `init_commands` otherwise. The gate at
  `acp/mod.rs:2218` stops being "initialize said something" and becomes "we have a list".

This is about fifteen lines. It is not free, as first assumed, but it is the only way the
running-session correction exists at all, and it also fixes a silent hole: an agent that
advertises its commands only after `session/new` is invisible to Zeron today.

### The harness probe

`Harness::commands` stops caching and becomes a plain probe:

```rust
async fn commands(&self, cwd: Option<&str>) -> Result<Vec<SlashCommand>, HarnessError>
```

- The `OnceCell` at `acp/mod.rs:548` is deleted.
- `discover_commands` passes the cwd to `session/new` only. It does **not** set the child
  process directory. `spawn_agent` calls `current_dir` (`acp/mod.rs:734-736`), and a
  missing directory then fails the spawn with `ErrorKind::NotFound`, which maps to
  `HarnessError::NotInstalled` (`acp/mod.rs:741-744`). A deleted worktree would report
  "adapter not installed" and never reach the retry below. The adapter resolves skills from
  the session cwd, so the child's own directory buys nothing.
- With a `cwd` supplied, the probe always opens a session. Today it skips `session/new`
  whenever initialize advertised commands (`acp/mod.rs:780-781`). That shortcut would make
  the new cwd dead for any agent that answers initialize, and would fill every cwd key with
  one identical list.
- `cwd: None` means `$HOME`. That is today's behavior, kept for callers with no workspace.
- The trait default still returns an empty list for harnesses whose wire carries no
  listing. Since #160 the native `claude` and `codex` drivers override it with their own
  per-harness discovery; they accept the `cwd` and ignore it. Scoping a native probe to a
  workspace is a feature of its own, not part of this change — see Non-goals.

`discover_models` keeps its own `OnceCell` and its `$HOME` cwd. Models are not treated as
workspace-scoped in this spec. See Non-goals.

### The engine cache

New file: `crates/engine/src/commands.rs`.

The cache lives in the engine, not the harness, because the two sources arrive in two
different places. The probe result returns inside the harness. The live event arrives in
the engine run loop at `sessions.rs:1572-1596`, where `run_cwd` is already in scope. One
cache in the engine is fed by both, and the harness stays a thin protocol client.

| Policy | Value |
|---|---|
| Key | `(HarnessId, String)`, path normalized |
| Fresh TTL | 10 minutes |
| Negative TTL | 30 seconds |
| Bound | LRU, 16 entries |
| Concurrency | single-flight per key; waiters subscribe to the in-flight probe |
| Live write | `AvailableCommands` overwrites `(harness, run_cwd)` and resets its TTL |

Key normalization reuses `expand_home` and trims trailing separators. `expand_home` is
private to `sessions.rs:1032-1042` today, so it moves or becomes `pub(crate)`.

Only the RPC side needs the expansion. The run path already expands at
`sessions.rs:298` ("expand it here, on the host, where the run spawns") before `drive_run`
captures `run_cwd` (`sessions.rs:1072`), so the live write always carries an absolute path.
The popup can still send `~` for a project-less chat. Both writers must land on one key.

The live write keys by `run_cwd`. For the ACP harness this is not a real fork in the road:
`SessionStarted` carries `request.cwd` verbatim (`acp/mod.rs:2208`), so the event's cwd and
the request's cwd are the same value. The contrasting rule at `sessions.rs:1577`, which
scopes a stored session id by the event's own cwd, does not apply here.

Entry states are explicit, which makes single-flight testable:

- `Fresh { commands, at }`
- `Failed { error, at }`
- `InFlight { subscribers }`

Read rules:

- `InFlight` waits on the in-flight probe.
- A stale entry counts as a miss and probes. The engine never answers with a stale list,
  because it has no way to push a correction afterwards. Freshness on the wire keeps the
  UI's own stale-while-revalidate render honest.

Write and eviction rules, because these are the cases the unit tests exist to pin:

- A live write onto `InFlight` resolves the waiters with the live list and marks the entry
  `Fresh`. The live list came from a real session in that cwd, so it is at least as good as
  the probe's.
- A probe result that lands on an entry already made `Fresh` by a later live write is
  discarded. Newest write wins, compared by timestamp, never by arrival order.
- LRU eviction skips `InFlight` entries. Evicting one would orphan its waiters.
- The cache lock is never held across an await. A read takes the lock, decides, and drops
  it before probing or waiting.

### The RPC

`ListCommands` gets its own params struct instead of borrowing `ListModelsParams`:

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListCommandsParams {
    harness: HarnessId,
    cwd: Option<String>,
}
```

The struct carries no `targetDeviceId`. Forwarding reads that field from the raw params
before any parse (`rpc.rs:986-992`), and `LIST_COMMANDS` is already forwardable
(`rpc.rs:766-772`), so the new `cwd` rides to the host device with no routing work.

`cwd` is optional. An engine on an older device ignores the unknown field and answers with
its `$HOME` list, so version skew degrades to today's behavior instead of failing.

### The composer

`slash_cache` is keyed by `(HarnessId, Option<DeviceId>, String)`. The device belongs in the
key because the popup already targets a device (`composer.rs:4032-4044`), and two devices
share the same path string for every project-less chat. Without it, one device's list
renders for a chat hosted on another.

The cwd resolves when the popup opens:

- selected chat: `chat.cwd`, or `~` when it is `None` (the field is optional,
  `composer.rs:4414-4418`)
- new chat: the space path, or `~` when project-less
- the checkout plan is ignored

A worktree is a checkout of the same repo, so the space path is the right answer for a
`NewWorktree` plan that has no directory yet, and close enough for `ReuseWorktree`. A
worktree that lacks untracked skills self-corrects through the live event once the session
runs.

Rendering is **stale while revalidate**. A cached entry renders at once with no spinner,
and a background `ListCommands` refreshes it. Changing the harness or the project picker
changes the key, so the list follows the project.

The composer cache has no TTL of its own. Every popup open sends one `ListCommands`, and
the engine decides whether that costs a probe. One expiry policy, in one place.

The stale comment at `parts.rs:339-343` is corrected, because the event now does feed a
cache.

## Error handling

| Case | Behavior |
|---|---|
| Probe fails (adapter missing, timeout, auth) | Existing `slash_error_message` path. Cache `Failed` for 30 seconds. |
| `session/new` rejects the cwd (deleted worktree, bad path) | Retry once with `$HOME`, then cache and show that list. The user keeps built-in commands. |
| Remote device runs an older engine | It ignores `cwd` and returns the `$HOME` list. No error. |
| No harness resolved yet | Unchanged. Empty popup, no fetch. |
| First open, nothing cached | Unchanged loading state. |

## Probe cost

A probe is not free. Measured on `claude-agent-acp` 0.66.0:

- it starts a full `claude` process,
- it runs the project's SessionStart hooks,
- it leaves a bare session directory under `~/.claude/projects/<slug>/`.

The current `$HOME` probes have left 666 bare directories and 7.9 MB in
`~/.claude/projects/-Users-<user>/`. After this change that litter moves into real project
directories. It stays invisible to `/resume`, because there is no transcript file, but it
is untidy.

Four mitigations are in the design: the 10 minute TTL, single-flight, negative caching, and
probing only on popup open. A running chat never probes at all, because its own event feeds
the cache.

The clean fix belongs upstream: a capabilities handshake that lists commands without
`session/new`. Record it as an adapter ask. Do not block this work on it.

## Testing

**Unit, `engine/src/commands.rs`**
- key normalization: `~`, trailing separator, two spellings of one path
- a stale entry is treated as a miss, not served
- TTL expiry and negative TTL
- LRU bound at 16 entries, and eviction skipping `InFlight`
- single-flight: two concurrent reads of one cold key produce one probe
- a live write onto `InFlight` resolves the waiters, and the late probe result is discarded

**Harness, in `crates/harness/tests/acp.rs`**
- the `fake-acp.sh` fixture asserts `session/new` receives the requested cwd
- a rejected cwd triggers exactly one `$HOME` retry
- a fixture that sends `available_commands_update` immediately after the `session/new`
  response produces one `AgentEvent::AvailableCommands` in the run's event stream. This is
  the regression test for the dropped notification, and it fails against today's code.
- the existing `commands_discovery_scans_the_initialize_response` (line 580) asserts that a
  second call is served from cache. Deleting the `OnceCell` invalidates that assertion, so
  the test loses its caching half. The caching contract moves to the engine unit tests.

**Engine**
- an `AvailableCommands` event during a run writes `(harness, run_cwd)`
- a later `ListCommands` for that cwd returns it with no probe

**UI**
The ui crate has no `TestAppContext` or `gpui::test` coverage today, so a test that drives
the popup is not writable against the current infrastructure. Rather than add a gpui test
harness for this change, cwd resolution is factored into a pure function that takes the
selected chat row, the selected space row, and the device id, and returns the cache key.
The tests cover that function, next to the existing pure-function tests at
`composer.rs:5648`. Anything beyond it is covered by the manual E2E below.

**Manual E2E**
- open a project with installed project skills, type `/`, confirm they appear
- open a project without them, confirm they do not
- the two-cwd probe above is the reproduction, and it gives the exact expected diff

## Non-goals

- **Models.** They keep the `$HOME` probe. `ListCommandsParams` and the cache key are
  shaped so models can join later without another interface change.
- **Per-workspace discovery for the native drivers.** #160 gave `claude` (an `initialize`
  control request) and `codex` (`skills/list`) their own probes, each cached per harness.
  Both now take the `cwd` and ignore it, so the engine cache keys their answer per
  workspace while the answer itself is still workspace-blind. Teaching those two wires to
  scope by directory is separate work; the interface is already in place for it.
- **File watchers on `.claude`.** The TTL plus the live event covers the real workflow.
- **Cleaning the 666 stale directories.** Separate chore.
- **Changing how an agent resolves skills.** Symlinked skills work correctly once the cwd
  is right.
