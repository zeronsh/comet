# Durable session publication

A remote agent can complete a turn while every peer parks its updates on missing
Loro dependencies. In the observed recurrence, 136 previous-writer cleanup
operations were absent from desktop; the next writer depended on their final
operation. Fetching the same older checkpoint could never supply that history.

Two lifecycle paths could strand local operations: LRU eviction dropped the
in-memory pending queue, and runtime shutdown disconnected the local-update
subscription before draining agents. A nonzero download cursor suppressed
republication when reopening. Snapshot replay proved the causal blockage;
historical relay/drop records were insufficient to attribute the original loss
to one specific lifecycle event.

The SQLite `chat_outbox` now journals each local update before network admission.
Stable batch IDs survive client recreation and make lost-ACK replay idempotent.
Opening a document imports pending updates before attaching writers, including
updates newer than its last debounced snapshot. The local subscription survives
network disconnection through agent cleanup. Cold-open initialization completes
before exposing the writable handle. Durably queued operations can be evicted;
a failed disk write pins the in-memory copy and invalidates the legacy replay
marker atomically with subsequent snapshot saves, including cursor saves.

A one-time, transactional bootstrap journals the saved document's operation
history regardless of download cursor, repairing histories stranded by older
writers. History is divided into relay-sized operation ranges without rewriting
IDs, commands, epochs, or download cursors. This deliberately trades a one-time
upload for recovery without relying on an unverified relay frontier. For the
incident input, it produced 28 rows totaling 8,102,543 bytes. Subsequent opens
replay only outstanding batches.

Indivisible oversized or permanently rejected batches remain durable checkpoint
obligations. A periodic retry survives missed events and failed POSTs; only
rejections covered by a successfully uploaded snapshot retire. Checkpoint
frontiers are decoded from the actual captured bytes, preventing concurrent
writes from advertising a newer frontier than the snapshot.

No relay protocol change or session lineage migration is required. SQLite
migration 3 adds the outbox, its index, and the bootstrap marker. Older readers
can consume the same updates. Writers must run the fix to obtain these durability
guarantees; downgrading to a writer that does not journal updates reintroduces
its old failure modes. This does not claim protection from arbitrary relay
history deletion or prove that every possible checkpoint-pruning defect is fixed.

## Validation

`cargo test --locked -p zeron-sync --lib` covers the sync client and storage,
including cursor preservation, restart/deduplication, and injected outbox-write
failure followed by ordinary or cursor-bearing snapshot persistence.

`cargo test --locked -p zeron-engine --lib --test session_publication --test restart_resume --test local_profiles --test born_chat2_race --test codex_subagents --test message_queue --test transcript_salvage`
covers actual LRU eviction, disconnected cleanup before snapshot debounce,
engine restart/resume, command queues, and profile lifecycle. The Codex adapter
fixture reproduces a successful-but-invisible resumed turn, then validates full
transcript equality through real ChatClients and EngineChatSinks over a loopback
WebSocket relay. The relay withholds ACKs to test client recreation and rejects
the first checkpoint POST to verify retry and retirement after success.

The ignored `real_codex_restart_publication` test uses an installed authenticated
Codex executable selected by `SESSION_SYNC_CODEX`; `SESSION_SYNC_MODEL` defaults
to `gpt-6-astra`, with medium reasoning. It passed with two successful turns and
an assertion that both used the same native session. All engine profiles and
conversation working directories are temporary; no existing agent is interrupted.

The ignored `supplied_incident_snapshots_reconcile_via_durable_bootstrap` test
accepts `SESSION_SYNC_DESKTOP_SNAPSHOT` and `SESSION_SYNC_REMOTE_SNAPSHOT` paths.
It passed using actual incident inputs through the loopback relay, restoring
168 messages and exact transcript/command equality. Private snapshots and
transcript bodies are not checked in. CI runs the deterministic suite without
accounts or private inputs.
