# Slash commands: per-workspace discovery

Status: PLANNED · 2026-08-16 investigation (project skills missing from the composer popup).

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

A fourth fact shapes the design. A live session already produces the correct list.
`acp/mod.rs:2218-2222` emits `AgentEvent::AvailableCommands` from a session started in the
real cwd. `doc/src/parts.rs:339-343` drops it, and no engine code reads it. The comment
there claims the event "feeds the engine's per-harness command cache". No such cache
exists. The comment is stale.

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

### The harness probe

`Harness::commands` stops caching and becomes a plain probe:

```rust
async fn commands(&self, cwd: Option<&str>) -> Result<Vec<SlashCommand>, HarnessError>
```

- The `OnceCell` at `acp/mod.rs:548` is deleted.
- `discover_commands` passes the cwd to both `spawn_agent` and `session/new`.
- `cwd: None` means `$HOME`. That is today's behavior, kept for callers with no workspace.
- The trait default still returns an empty list for non-ACP harnesses.

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

Key normalization reuses `expand_home` (`sessions.rs:1032-1042`) and trims trailing
separators. The engine runs on the host device, so it expands `~` with the right home.
This matters for the live write: `run_cwd` is `request.cwd`, which can still hold `~`
(`sessions.rs:1072`). Both writers must produce the same key, or a running chat would fill
one entry while the popup reads another.

The live write keys by the **request** cwd, not by the cwd on the `SessionStarted` event.
The rule next door is the opposite: `sessions.rs:1577` scopes a stored session id by the
event's own cwd, because that is where the harness really created the session. The command
cache needs the other one. The popup looks up by `chat.cwd`, which is the request cwd, so
keying by the event would fill an entry that nothing ever reads.

Entry states are explicit, which makes single-flight testable:

- `Fresh { commands, at }`
- `Failed { error, at }`
- `InFlight { subscribers }`

A read that finds `InFlight` waits on it. A read that finds a stale entry treats it as a
miss and probes. The engine never answers with a stale list, because it has no way to push
a correction afterwards. Freshness on the wire keeps the UI's own stale-while-revalidate
render honest.

### The RPC

`ListCommands` gets its own params struct instead of borrowing `ListModelsParams`:

```rust
struct ListCommandsParams {
    harness: HarnessId,
    cwd: Option<String>,
    target_device_id: Option<String>,
}
```

`cwd` is optional. An engine on an older device ignores the unknown field and answers with
its `$HOME` list, so version skew degrades to today's behavior instead of failing.

### The composer

`slash_cache` is keyed by `(HarnessId, String)`. The cwd resolves when the popup opens:

- selected chat: `chat.cwd`
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
- LRU bound at 16 entries
- single-flight: two concurrent reads of one cold key produce one probe

**Harness, extending `crates/harness/tests/acp.rs:580`**
- the mock agent asserts `session/new` receives the requested cwd
- a rejected cwd triggers exactly one `$HOME` retry

**Engine**
- an `AvailableCommands` event during a run writes `(harness, run_cwd)`
- a later `ListCommands` for that cwd returns it with no probe

**UI**
- cwd resolution for the three cases: chat row, space row, project-less
- switching the project picker changes the cache key

**Manual E2E**
- open a project with installed project skills, type `/`, confirm they appear
- open a project without them, confirm they do not
- the two-cwd probe above is the reproduction, and it gives the exact expected diff

## Non-goals

- **Models.** They keep the `$HOME` probe. `ListCommandsParams` and the cache key are
  shaped so models can join later without another interface change.
- **File watchers on `.claude`.** The TTL plus the live event covers the real workflow.
- **Cleaning the 666 stale directories.** Separate chore.
- **Changing how an agent resolves skills.** Symlinked skills work correctly once the cwd
  is right.
