//! ChatClient behavior against a hand-driven server end (channel pipes — no
//! WebSocket): handshake precision, backfill, push/ack retirement, and the
//! reconnect re-push path. Virtual clock (`start_paused`) so backoff and
//! deadlines cost nothing.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::*;
use crate::chat_frames::{decode, encode, frame_type};

// ── plumbing: linked pipes + scripted connector ─────────────────────────────

struct ServerEnd {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

fn pipe_pair() -> (BinPipe, ServerEnd) {
    let (c2s_tx, c2s_rx) = mpsc::channel(64);
    let (s2c_tx, s2c_rx) = mpsc::channel(64);
    (
        BinPipe {
            tx: c2s_tx,
            rx: s2c_rx,
        },
        ServerEnd {
            tx: s2c_tx,
            rx: c2s_rx,
        },
    )
}

struct ChanConnector {
    pipes: Mutex<VecDeque<BinPipe>>,
}

impl BinConnector for ChanConnector {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>> {
        let pipe = lock(&self.pipes).pop_front();
        Box::pin(async move { pipe.ok_or(SyncError::Closed) })
    }
}

// ── sink + fetcher doubles ──────────────────────────────────────────────────

#[derive(Default)]
struct RecordingSink {
    rows: Mutex<Vec<(Vec<u8>, u64)>>,
    checkpoints: Mutex<Vec<(Vec<u8>, u64)>>,
    cursor_advances: Mutex<Vec<u64>>,
    frontier_contained: std::sync::atomic::AtomicBool,
}

impl ChatDocSink for RecordingSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) {
        lock(&self.rows).push((bytes.to_vec(), cursor));
    }
    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        lock(&self.checkpoints).push((bytes.to_vec(), cursor));
        Ok(())
    }
    fn contains_frontier(&self, _frontier: &[u8]) -> bool {
        self.frontier_contained
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn advance_cursor(&self, cursor: u64) {
        lock(&self.cursor_advances).push(cursor);
    }
}

struct FixedFetcher {
    bytes: Vec<u8>,
    calls: Arc<std::sync::atomic::AtomicU64>,
}

impl CheckpointFetcher for FixedFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let bytes = self.bytes.clone();
        Box::pin(async move { Ok(bytes) })
    }
}

// ── server-side script helpers ──────────────────────────────────────────────

async fn expect_kind(end: &mut ServerEnd, kind: u8) -> wire::WireFrame {
    let bytes = end.rx.recv().await.expect("client hung up");
    let frame = decode(&bytes).expect("client sent undecodable frame");
    assert_eq!(
        frame.kind, kind,
        "expected frame {kind:#x}, got {:#x}",
        frame.kind
    );
    frame
}

async fn send(end: &ServerEnd, kind: u8, header: serde_json::Value, payload: &[u8]) {
    end.tx.send(encode(kind, &header, payload)).await.unwrap();
}

/// Answer hello with `state`, then serve the rows request with `rows`.
/// Returns the observed `after` from the rows request. `expect_exclude`
/// pins the F1 rule: the process's FIRST backfill must redownload own rows
/// (false), same-process reconnects skip them (true).
async fn serve_join(
    end: &mut ServerEnd,
    state: serde_json::Value,
    frontier: &[u8],
    rows: Vec<(u64, &str, Vec<u8>)>,
    expect_exclude: bool,
) -> u64 {
    let hello = expect_kind(end, frame_type::HELLO).await;
    assert!(hello.header["device"].is_string());
    let head_seq = state["headSeq"].as_u64().unwrap();
    send(end, frame_type::STATE, state, frontier).await;
    let req = expect_kind(end, frame_type::ROWS_REQ).await;
    assert_eq!(req.header["excludeOwn"], expect_exclude);
    let after = req.header["after"].as_u64().unwrap();
    for (seq, device, bytes) in rows {
        send(
            end,
            frame_type::ROW,
            serde_json::json!({"seq": seq, "device": device, "batchId": format!("b{seq}")}),
            &bytes,
        )
        .await;
    }
    send(
        end,
        frame_type::ROWS_DONE,
        serde_json::json!({"headSeq": head_seq}),
        &[],
    )
    .await;
    after
}

fn connector(pipes: Vec<BinPipe>) -> Arc<ChanConnector> {
    Arc::new(ChanConnector {
        pipes: Mutex::new(pipes.into_iter().collect()),
    })
}

fn fetcher(bytes: &[u8]) -> (Arc<FixedFetcher>, Arc<std::sync::atomic::AtomicU64>) {
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        Arc::new(FixedFetcher {
            bytes: bytes.to_vec(),
            calls: calls.clone(),
        }),
        calls,
    )
}

// ── plan_catch_up (pure) ────────────────────────────────────────────────────

#[test]
fn catch_up_plan_covers_the_decision_table() {
    let state = |head: u64, ckpt: u64| wire::StateHeader {
        head_seq: head,
        seq_floor: ckpt,
        checkpoint_seq: ckpt,
        checkpoint_size: if ckpt > 0 { 1000 } else { 0 },
        row_count: 0,
        row_bytes: 0,
    };
    // Seeded-at-zero room (M1): checkpointSeq 0 but a real blob — the
    // presence test is SIZE; a fresh reader must fetch the seed.
    let seeded = wire::StateHeader {
        head_seq: 0,
        seq_floor: 0,
        checkpoint_seq: 0,
        checkpoint_size: 276_000,
        row_count: 0,
        row_bytes: 0,
    };
    assert_eq!(
        plan_catch_up(0, &seeded, false),
        CatchUpPlan::CheckpointThenRows { after: 0 }
    );
    assert_eq!(
        plan_catch_up(0, &seeded, true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
    // Empty room / no checkpoint: rows from the cursor.
    assert_eq!(
        plan_catch_up(0, &state(0, 0), true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
    assert_eq!(
        plan_catch_up(4, &state(9, 0), true),
        CatchUpPlan::RowsOnly { after: 4 }
    );
    // Frontier contained: skip the checkpoint even from an older cursor.
    assert_eq!(
        plan_catch_up(2, &state(9, 5), true),
        CatchUpPlan::RowsOnly { after: 5 }
    );
    assert_eq!(
        plan_catch_up(7, &state(9, 5), true),
        CatchUpPlan::RowsOnly { after: 7 }
    );
    // Frontier missing: checkpoint first, rows after it.
    assert_eq!(
        plan_catch_up(2, &state(9, 5), false),
        CatchUpPlan::CheckpointThenRows { after: 5 }
    );
    // Server lost state (cursor ahead of head): cursor is meaningless.
    assert_eq!(
        plan_catch_up(50, &state(3, 0), true),
        CatchUpPlan::RowsOnly { after: 0 }
    );
}

// ── end-to-end actor behavior ───────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn fresh_join_backfills_rows_and_advances_cursor() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, fetch_calls) = fetcher(b"");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 2, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 2, "rowBytes": 64}),
            &[],
            vec![(1, "dev-b", vec![0xaa]), (2, "dev-b", vec![0xbb])],
            false,
        )
        .await;
        assert_eq!(after, 0);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(
        *lock(&sink.rows),
        vec![(vec![0xaa], 1), (vec![0xbb], 2)],
        "both remote rows imported in seq order"
    );
    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    let stats = client.stats();
    assert!(stats.connected);
    assert_eq!(stats.cursor, 2);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn contained_frontier_skips_the_checkpoint_download() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    sink.frontier_contained
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (fetch, fetch_calls) = fetcher(b"never");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 8, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 160_000, "rowCount": 3, "rowBytes": 900}),
            &[1, 2, 3],
            vec![
                (6, "dev-b", vec![6]),
                (7, "dev-b", vec![7]),
                (8, "dev-b", vec![8]),
            ],
            false,
        )
        .await;
        // Client-side precision: cursor was 0 but the frontier is local —
        // skip straight past the checkpointed span.
        assert_eq!(after, 5);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(lock(&sink.checkpoints).is_empty());
    assert_eq!(lock(&sink.rows).len(), 3);
    assert_eq!(client.stats().cursor, 8);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn missing_frontier_fetches_and_imports_the_checkpoint_first() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, fetch_calls) = fetcher(b"checkpoint-bytes");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 6, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 16, "rowCount": 1, "rowBytes": 10}),
            &[9, 9, 9],
            vec![(6, "dev-b", vec![6])],
            false,
        )
        .await;
        assert_eq!(after, 5, "rows resume after the checkpoint");
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        2,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    server.await.unwrap();

    assert_eq!(fetch_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        *lock(&sink.checkpoints),
        vec![(b"checkpoint-bytes".to_vec(), 5)]
    );
    assert_eq!(*lock(&sink.rows), vec![(vec![6u8], 6)]);
    client.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn unacked_pushes_survive_reconnect_and_acks_retire_them() {
    let (pipe1, mut end1) = pipe_pair();
    let (pipe2, mut end2) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");

    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    let s1 = tokio::spawn({
        let state = empty_state.clone();
        async move {
            serve_join(&mut end1, state, &[], vec![], false).await;
            // Receive the push but die before acking — the client must
            // re-push the SAME batch id on the next session.
            let push = expect_kind(&mut end1, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            assert_eq!(push.payload, vec![0xd1u8]);
            drop(end1); // socket dies
            batch_id
        }
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe1, pipe2]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");

    client.enqueue_update(vec![0xd1]);
    let first_batch = s1.await.unwrap();
    assert_eq!(
        client.stats().pending_pushes,
        1,
        "unacked batch stays queued"
    );

    // Second session: same handshake, then the replayed push gets acked.
    let s2 = tokio::spawn({
        let state = empty_state.clone();
        async move {
            serve_join(&mut end2, state, &[], vec![], true).await;
            let push = expect_kind(&mut end2, frame_type::PUSH).await;
            let batch_id = push.header["batchId"].as_str().unwrap().to_string();
            send(
                &end2,
                frame_type::ACK,
                serde_json::json!({"batchId": batch_id, "seq": 1, "dup": false}),
                &[],
            )
            .await;
            (batch_id, end2)
        }
    });
    let (replayed_batch, _keep_alive) = s2.await.unwrap();
    assert_eq!(
        replayed_batch, first_batch,
        "reconnect replays the same batch id"
    );

    // Ack lands asynchronously — wait for the pending queue to drain.
    let mut events = client.events();
    while client.stats().pending_pushes > 0 {
        let _ = events.recv().await;
    }
    assert_eq!(client.stats().cursor, 1, "ack advanced the cursor");
    assert_eq!(*lock(&sink.cursor_advances), vec![1]);
    client.shutdown().await;
}

// ── 2026-08-10 review fixes (F1–F4) ─────────────────────────────────────────

struct PendingFetcher;
impl CheckpointFetcher for PendingFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        Box::pin(std::future::pending())
    }
}

/// F2: a permanent server verdict (`too_large`) retires the batch from the
/// replay queue; a transient one (`quota`) keeps it and re-pushes on the
/// retry clock without waiting for a new enqueue.
#[tokio::test(start_paused = true)]
async fn permanent_rejection_retires_transient_keeps_and_retries() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    let server = tokio::spawn(async move {
        serve_join(&mut end, empty_state, &[], vec![], false).await;
        // First batch: permanently rejected.
        let doomed = expect_kind(&mut end, frame_type::PUSH).await;
        let doomed_id = doomed.header["batchId"].as_str().unwrap().to_string();
        send(
            &end,
            frame_type::ERROR,
            serde_json::json!({"code": "too_large", "message": "push rejected", "batchId": doomed_id}),
            &[],
        )
        .await;
        // Second batch: quota-limited once, then replayed by the retry clock
        // (no further enqueue nudges) and acked.
        let quotad = expect_kind(&mut end, frame_type::PUSH).await;
        let quotad_id = quotad.header["batchId"].as_str().unwrap().to_string();
        send(
            &end,
            frame_type::ERROR,
            serde_json::json!({"code": "quota", "message": "later", "batchId": quotad_id}),
            &[],
        )
        .await;
        let replay = expect_kind(&mut end, frame_type::PUSH).await;
        assert_eq!(
            replay.header["batchId"].as_str().unwrap(),
            quotad_id,
            "retry clock replays the SAME quota-limited batch"
        );
        send(
            &end,
            frame_type::ACK,
            serde_json::json!({"batchId": quotad_id, "seq": 1, "dup": false}),
            &[],
        )
        .await;
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");

    let mut events = client.events();
    client.enqueue_update(vec![0xd0]); // doomed
    // Retirement lands asynchronously; PushRejected marks it.
    loop {
        if let Ok(ChatEvent::PushRejected) = events.recv().await {
            break;
        }
    }
    assert_eq!(client.stats().pending_pushes, 0, "doomed batch retired");

    client.enqueue_update(vec![0xb0]);
    let _keep = server.await.unwrap();
    while client.stats().pending_pushes > 0 {
        let _ = events.recv().await;
    }
    assert_eq!(client.stats().cursor, 1, "quota batch eventually landed");
    assert!(client.stats().rejected >= 2);
    client.shutdown().await;
}

/// F2: batches over the row cap never enter the replay queue.
#[tokio::test(start_paused = true)]
async fn oversized_enqueue_is_refused_at_the_door() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});
    let server = tokio::spawn(async move {
        serve_join(&mut end, empty_state, &[], vec![], false).await;
        end
    });
    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    // Exactly the DO row cap (1 MiB): the frame header would push the WS
    // message over the runtime's 1 MiB cap and the socket would close with
    // NO error frame to retire the batch — the gate must refuse it (this is
    // why MAX_PUSH_BYTES carries headroom below the row cap).
    client.enqueue_update(vec![0u8; 1024 * 1024]);
    let stats = client.stats();
    assert_eq!(stats.pending_pushes, 0, "boundary batch not queued");
    assert_eq!(stats.rejected, 1);
    client.shutdown().await;
}

/// F4 (second half): `shutdown()` must complete promptly even while the
/// actor is parked inside a hung checkpoint fetch.
#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_hung_checkpoint_fetch() {
    let (pipe1, mut end1) = pipe_pair();
    let (pipe2, mut end2) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let empty_state = serde_json::json!({"headSeq": 0, "seqFloor": 0,
        "checkpointSeq": 0, "checkpointSize": 0, "rowCount": 0, "rowBytes": 0});

    // Session 1: clean join (no checkpoint), then the socket dies.
    let s1 = tokio::spawn(async move {
        serve_join(&mut end1, empty_state, &[], vec![], false).await;
        drop(end1);
    });
    // Session 2: a checkpoint appeared — the client must fetch, and hangs.
    let s2 = tokio::spawn(async move {
        let _hello = expect_kind(&mut end2, frame_type::HELLO).await;
        send(
            &end2,
            frame_type::STATE,
            serde_json::json!({"headSeq": 9, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 1000, "rowCount": 4, "rowBytes": 40}),
            &[7, 7, 7],
        )
        .await;
        end2 // keep the pipe alive; only the fetch is stuck
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe1, pipe2]),
        sink,
        Arc::new(PendingFetcher),
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("first join succeeds");
    s1.await.unwrap();
    let _keep = s2.await.unwrap();
    // Let the actor redial and park inside the hung fetch.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(30), client.shutdown())
        .await
        .expect("shutdown must not hang on a stuck fetch");
}

/// F3: a server whose headSeq fell behind our cursor (reset/wiped room) is
/// SURFACED — counted in stats, honest head_seq — not silently absorbed.
#[tokio::test(start_paused = true)]
async fn server_reset_is_counted_and_head_seq_stays_honest() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default());
    let (fetch, _) = fetcher(b"");
    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 3, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 0, "rowCount": 3, "rowBytes": 30}),
            &[],
            vec![
                (1, "dev-b", vec![1]),
                (2, "dev-b", vec![2]),
                (3, "dev-b", vec![3]),
            ],
            false,
        )
        .await;
        assert_eq!(after, 0, "meaningless cursor treated as fresh");
        end
    });
    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        fetch,
        "dev-a",
        50, // persisted cursor from before the room was wiped
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    let stats = client.stats();
    assert_eq!(stats.server_resets, 1, "reset visible to the host");
    assert_eq!(stats.head_seq, 3, "server view not masked by the cursor");
    assert_eq!(stats.cursor, 3, "cursor re-anchored by the backfill");
    client.shutdown().await;
}

/// F4: a checkpoint fetch that never resolves fails the first join within
/// the deadline instead of hanging the actor (and shutdown) forever.
#[tokio::test(start_paused = true)]
async fn hung_checkpoint_fetch_fails_the_join_within_deadline() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let server = tokio::spawn(async move {
        let hello = expect_kind(&mut end, frame_type::HELLO).await;
        assert!(hello.header["device"].is_string());
        send(
            &end,
            frame_type::STATE,
            serde_json::json!({"headSeq": 9, "seqFloor": 5, "checkpointSeq": 5,
                "checkpointSize": 1000, "rowCount": 4, "rowBytes": 40}),
            &[7, 7, 7],
        )
        .await;
        end // keep the pipe alive; the fetch is what must time out
    });
    let joined = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink,
        Arc::new(PendingFetcher),
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await;
    assert!(joined.is_err(), "hung fetch must not hang the join");
    let _keep = server.await.unwrap();
}

/// M1 seed shape: checkpointSeq 0 with a real blob. BOTH presence tests
/// (plan_catch_up AND run_session's frontier short-circuit) must key on
/// SIZE — the 2026-08-10 gauntlet caught seq==0 short-circuits in each,
/// which would have made every adopted reader skip the seed and render an
/// EMPTY transcript.
#[tokio::test(start_paused = true)]
async fn seeded_at_zero_room_fetches_the_checkpoint() {
    let (pipe, mut end) = pipe_pair();
    let sink = Arc::new(RecordingSink::default()); // frontier NOT contained
    let (fetch, fetch_calls) = fetcher(b"seed-checkpoint-bytes");

    let server = tokio::spawn(async move {
        let after = serve_join(
            &mut end,
            serde_json::json!({"headSeq": 0, "seqFloor": 0, "checkpointSeq": 0,
                "checkpointSize": 276_342, "rowCount": 0, "rowBytes": 0}),
            &[7, 7, 7], // non-empty frontier the fresh doc can't contain
            vec![],
            false,
        )
        .await;
        assert_eq!(after, 0);
        end
    });

    let client = ChatClient::connect_with_tuned(
        connector(vec![pipe]),
        sink.clone(),
        fetch,
        "dev-a",
        0,
        ChatTuning::default(),
    )
    .await
    .expect("join succeeds");
    let _keep = server.await.unwrap();

    assert_eq!(
        fetch_calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "seed checkpoint fetched despite checkpointSeq == 0"
    );
    assert_eq!(
        *lock(&sink.checkpoints),
        vec![(b"seed-checkpoint-bytes".to_vec(), 0)]
    );
    client.shutdown().await;
}
