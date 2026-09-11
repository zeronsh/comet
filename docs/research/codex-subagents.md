# Codex subagent transcripts

The Codex adapter uses the original spawn call id as the stable owner of each
child thread. The engine keeps its existing document format:
`{chatId}--sub--{spawnCallId}`. No document schema migration or historical
transcript merging is performed.

## Wire contracts

- v1 publishes `collabAgentToolCall` with `tool: spawnAgent`. The child id can
  appear only in the completed item's `receiverThreadIds`. Spawn cards appear
  once; `sendInput`, `wait`, `resumeAgent` and `closeAgent` remain control tools.
- v2 publishes `subAgentActivity`. `kind: started` (or `spawned`) establishes
  ownership. Later activities have independent ids, including
  `subagent-completed-{turnId}`. They must not create cards or replace ownership.
- Child content arriving before registration is buffered and emitted after the
  parent card exists. The unbound event backlog has a 4 MiB aggregate limit;
  overflow is logged and excess events are dropped.
- Child turn completion settles the assignment. Parent activity completion
  must not truncate child output. Duplicate message and turn completions are
  ignored; completion-only assistant messages retain their full text.
- A follow-up can start a new child turn without a `userMessage` echo (observed
  with v2). A tagged `Steered` boundary reopens the same document and updates
  its card to running without fabricating user text. Actual child user messages
  are still persisted when supplied by the provider.
- On `thread/resume`, stored parent spawn items rebuild the child ownership
  table without replaying cards or content into Zeron's existing documents.
- Root self-activity never registers a child or produces a spawn card.

## Offline regression coverage

Fixtures in `crates/harness/tests/fixtures/codex/` use anonymized ids and short
test content. They cover the 0.153.4 v1/v2 shapes, early output, distinct activity
ids, completion-only messages, follow-ups, duplicate terminals and resume.

`crates/engine/tests/codex_subagents.rs` runs the actual adapter against the fake
app-server, verifies two persisted documents for two children, checks parent
content isolation and final card status, then restarts the engine and appends a
resumed child's output to the original document.

```sh
cargo test --locked -p zeron-harness -p zeron-doc -p zeron-proto
cargo test --locked -p zeron-engine --lib --test e2e --test codex_subagents
```

## Live validation

Codex CLI 0.153.4 was checked on 2026-09-10. The local model catalog advertises
`multi_agent_version: v1` for `gpt-5.6-luna` and `v2` for `gpt-5.6-sol` and
`gpt-5.6-terra`. Feature flags alone do not force a v2 model onto the v1 protocol.
Setting `agents.enabled=false` disables agent tools, so it cannot select v1.

The ignored test `live_subagent_spawn_and_followup_keep_one_transcript` asks one
child for three short replies: initial assignment, follow-up, and another
follow-up after restarting the app-server and resuming the parent. It asserts
one original spawn, one owner for all child output, and three child completions.
It uses a temporary working directory and consumes real model calls.
Both modes passed, including the restart. With v1, the parent must reactivate
the existing child using `resume_agent` before `send_input` after the app-server
restarts; otherwise Codex reports `notFound`. The v2 `followup_task` reactivates
the child directly. Neither operation establishes a new transcript owner.

Supply an executable wrapper that forwards `"$@"` to the installed Codex CLI.
For v1, the validated flags are `-c features.multi_agent=true
-c features.multi_agent_v2=false -c project_doc_max_bytes=0`; for v2, use
`-c features.multi_agent=true -c features.multi_agent_v2=true
-c agents.enabled=true -c project_doc_max_bytes=0`. These are per-process
overrides; the test does not edit the user's Codex config.

```sh
CODEX_SUBAGENT_TEST_MODE=v1 \
CODEX_SUBAGENT_TEST_MODEL=gpt-5.6-luna \
CODEX_SUBAGENT_TEST_EXECUTABLE=/absolute/path/to/v1-wrapper \
cargo test --locked -p zeron-harness --test codex \
  live_subagent_spawn_and_followup_keep_one_transcript -- --ignored --nocapture
```

Repeat with mode `v2`, model `gpt-5.6-sol` and the v2 wrapper. Optionally set
`CODEX_SUBAGENT_TEST_CAPTURE` to save normalized events for inspection. Model
availability and advertised agent versions should be checked again on upgrades.
