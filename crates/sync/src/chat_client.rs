//! ChatClient — WebSocket transport for chat2 rooms (docs/chat2-sync.md C1):
//! hello/state handshake with client-side checkpoint precision, cursor-based
//! row backfill, push/ack with a pending-unacked queue, opaque presence
//! relay, probe/redial liveness, and reconnect with exponential backoff.
//!
//! The client owns no CRDT semantics: update bytes flow through a
//! [`ChatDocSink`] the engine implements over its `ChatDocHandle` (import +
//! persist doc AND cursor in one transaction — the C2 rule). Wire frames are
//! the binary chat2 codec ([`crate::chat_frames`]), byte-compatible with
//! `edge/src/chat-frames.ts`.
//!
//! Liveness discipline is inherited from `registry.rs` and its incidents:
//! transport pings prove nothing about the DO; room health is judged only by
//! protocol frames with probe deadlines.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::chat_frames::{self as wire, frame_type};
use crate::types::{StaticUrl, SyncError, UrlProvider};

const PING_INTERVAL: Duration = Duration::from_secs(15);
const SILENCE_LEASE: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HELLO_DEADLINE: Duration = Duration::from_secs(15);
/// Backfill after hello must complete (rowsDone) within this deadline —
/// post-strip rooms are KB-scale, so this is generous even at 1.2 Mbps.
const BACKFILL_DEADLINE: Duration = Duration::from_secs(120);
const PROBE_DEADLINE: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Quiet-room probe cadence default (matches the registry's fleet math).
const PROBE_QUIET_DEFAULT: Duration = Duration::from_secs(900);
/// A checkpoint fetch that hasn't finished by now is treated as a dead link
/// and the session redials (the fetch itself is Range-resumable, so a retry
/// picks up where the bytes stopped). Sized for MAX_CHECKPOINT_BYTES over
/// the 1.2 Mbps links this design exists for.
const CHECKPOINT_FETCH_DEADLINE: Duration = Duration::from_secs(120);
/// Re-push cadence after a `quota` rejection (server window is 60 s; pending
/// batches must not wait for the next enqueue/probe to retry).
const QUOTA_RETRY: Duration = Duration::from_secs(5);
/// Client-side push cap: the DO's per-row cap (`chat-log.ts MAX_ROW_BYTES`,
/// 1 MiB) minus frame-overhead headroom. The headroom matters: the runtime
/// closes WS messages at 1 MiB BEFORE the DO runs, so a payload within a
/// frame-header's width of the row cap would die with no error frame (and no
/// batchId to retire) — the silent replay-forever wedge, again. Enforced at
/// enqueue: a batch the server can never accept must not enter the replay
/// queue.
pub const MAX_PUSH_BYTES: usize = 1024 * 1024 - 4096;

/// Per-client tuning.
#[derive(Clone, Copy, Debug)]
pub struct ChatTuning {
    pub probe_quiet: Duration,
}

impl Default for ChatTuning {
    fn default() -> Self {
        Self {
            probe_quiet: PROBE_QUIET_DEFAULT,
        }
    }
}

/// Connection/sync lifecycle notifications (best-effort broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatEvent {
    /// Joined (or re-joined); the hello state has been received.
    Connected,
    /// Backfill finished — the doc is converged with the room at this head.
    CaughtUp { head_seq: u64 },
    /// Remote rows/acks were applied through the sink — republish.
    Applied,
    /// The connection dropped; the client is backing off before redialing.
    Disconnected,
    /// A remote device's presence beat arrived.
    Presence,
    /// The server's headSeq is behind our persisted cursor — the room was
    /// reset/wiped. The catch-up treats the cursor as fresh; the HOST should
    /// react by re-seeding via checkpoint (chat-room.ts `/reset` recovery).
    ServerReset,
    /// A queued batch was permanently rejected (or refused at enqueue) and
    /// dropped from the replay queue. The ops remain in the local doc; the
    /// row-path for them is gone, so they reach peers only when THIS device
    /// next posts a checkpoint — the C3 host should treat this event as a
    /// checkpoint trigger, not a shrug.
    PushRejected,
}

// ── engine-facing traits ────────────────────────────────────────────────────

/// Where remote bytes land. The engine implements this over its doc handle;
/// every method persists doc content AND the room cursor in one transaction
/// (`DocsStore::save_snapshot_with_cursor`) so they can never diverge.
pub trait ChatDocSink: Send + Sync + 'static {
    /// Import one remote update row; `cursor` is the row's seq.
    fn apply_row(&self, bytes: &[u8], cursor: u64);
    /// Replace/merge from a checkpoint blob; `cursor` is its checkpointSeq.
    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String>;
    /// Client-side precision (replaces the server VV diff): is the server
    /// checkpoint's frontier already contained in the local doc?
    fn contains_frontier(&self, frontier: &[u8]) -> bool;
    /// An own-write ack advanced the cursor with no content change.
    fn advance_cursor(&self, cursor: u64);
}

/// `GET /chat2/{chatId}/checkpoint` over HTTP. Implementations should resume
/// partial downloads with `Range: bytes=N-` (the DO serves 206) — that
/// resumability is the point of checkpoint-over-HTTP vs export-per-join.
pub trait CheckpointFetcher: Send + Sync + 'static {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>>;
}

// ── catch-up planning (pure — the client-side precision rule) ───────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpPlan {
    /// Local doc already contains the checkpoint frontier (or there is no
    /// checkpoint): stream rows only.
    RowsOnly { after: u64 },
    /// Fetch + import the checkpoint first, then rows after it.
    CheckpointThenRows { after: u64 },
}

/// Decide the catch-up path from the hello state. `frontier_contained` is the
/// sink's verdict on the checkpoint frontier payload.
pub fn plan_catch_up(
    cursor: u64,
    state: &wire::StateHeader,
    frontier_contained: bool,
) -> CatchUpPlan {
    // A cursor ahead of the server means the server lost state (reset/wipe);
    // our cursor is meaningless there — treat as fresh.
    let cursor = if cursor > state.head_seq { 0 } else { cursor };
    // Presence test is the SIZE, not the seq: a freshly SEEDED room's
    // checkpoint legitimately covers seq 0 (M1 seeds before any rows
    // exist), and seq==0 misread as "no checkpoint" made every adopted
    // reader skip the seed and render an empty transcript (caught by the
    // 2026-08-10 cutover gauntlet).
    if state.checkpoint_size == 0 {
        return CatchUpPlan::RowsOnly { after: cursor };
    }
    if frontier_contained {
        // Rows ≤ checkpointSeq are covered by a checkpoint we already
        // contain — skip straight past them even if our cursor is older.
        CatchUpPlan::RowsOnly {
            after: cursor.max(state.checkpoint_seq),
        }
    } else {
        CatchUpPlan::CheckpointThenRows {
            after: state.checkpoint_seq,
        }
    }
}

// ── transport plumbing (binary sibling of registry.rs's TextPipe) ───────────

pub(crate) struct BinPipe {
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
    pub(crate) rx: mpsc::Receiver<Vec<u8>>,
}

pub(crate) trait BinConnector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>>;
}

struct WsBinConnector {
    url: Arc<dyn UrlProvider>,
}

impl BinConnector for WsBinConnector {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>> {
        let provider = self.url.clone();
        Box::pin(async move {
            let url = provider.url().await?;
            let ws = crate::dial::connect_ws(&url)
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            tokio::spawn(pump(ws, out_rx, in_tx));
            Ok(BinPipe {
                tx: out_tx,
                rx: in_rx,
            })
        })
    }
}

/// Shuttle binary frames between the WebSocket and the actor's channels; the
/// text `"ping"` keepalive rides the same socket (runtime-answered pair).
async fn pump(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    last_rx = tokio::time::Instant::now();
                    if in_tx.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {
                    // Text pong / control frames: transport liveness only.
                    last_rx = tokio::time::Instant::now();
                }
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                tracing::warn!("chat2 socket silent past lease; treating as dead");
                break;
            }
        }
    }
}

// ── shared client state ─────────────────────────────────────────────────────

struct PendingPush {
    batch_id: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    cursor: u64,
    pending: VecDeque<PendingPush>,
    /// Last hello/probe view of the server log (checkpoint-policy inputs).
    server: Option<wire::StateHeader>,
    /// Set by a transient (`quota`) rejection: re-push at this instant
    /// instead of waiting for the next enqueue/probe/reconnect.
    retry_at: Option<tokio::time::Instant>,
    /// True while draining a quota-rejected queue. Retry ticks then probe
    /// with the HEAD batch only (a full-queue replay would itself consume
    /// the server's quota window — N pending × 12 ticks/window livelocks
    /// past N≈25), and each ack immediately re-arms the clock until the
    /// queue empties.
    quota_blocked: bool,
}

/// `zeron sync` surface (plan: cursor / headSeq / floorLag / pendingPushes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatStatsSnapshot {
    pub connected: bool,
    pub cursor: u64,
    pub head_seq: u64,
    pub seq_floor: u64,
    pub checkpoint_seq: u64,
    /// Byte size of the room's stored checkpoint (0 = none). The host's
    /// bootstrap heal keys off this: a room with rows but NO checkpoint
    /// cannot cover its rows' causal deps for cold readers.
    pub checkpoint_size: u64,
    pub row_count: u64,
    pub row_bytes: u64,
    pub pending_pushes: u64,
    pub rejoins: u64,
    pub disconnects: u64,
    pub rejected: u64,
    /// Times a hello found the server behind our cursor (room reset/wiped).
    /// Nonzero means the host owes the room a re-seed checkpoint.
    pub server_resets: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the client ──────────────────────────────────────────────────────────────

/// A live chat2-room membership for one chat doc.
pub struct ChatClient {
    shared: Arc<Mutex<Shared>>,
    events: broadcast::Sender<ChatEvent>,
    shutdown: watch::Sender<bool>,
    nudge: mpsc::Sender<()>,
    probe: mpsc::Sender<()>,
    redial: mpsc::Sender<()>,
    presence_out: mpsc::Sender<(i64, Vec<u8>)>,
    flags: Arc<Flags>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct Flags {
    connected: std::sync::atomic::AtomicBool,
    rejoins: std::sync::atomic::AtomicU64,
    disconnects: std::sync::atomic::AtomicU64,
    rejected: std::sync::atomic::AtomicU64,
    server_resets: std::sync::atomic::AtomicU64,
}

impl ChatClient {
    /// Connect (fixed URL — dev/tests).
    pub async fn connect(
        url: &str,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
    ) -> Result<Self, SyncError> {
        Self::connect_via(
            Arc::new(StaticUrl(url.to_string())),
            sink,
            fetcher,
            device_id,
            initial_cursor,
        )
        .await
    }

    /// Connect with a per-dial URL provider (fresh `?token=` every attempt).
    /// Resolves once hello/state lands AND the initial catch-up (checkpoint
    /// if needed + row backfill) completes; first-attempt failures are `Err`
    /// (callers own the initial-join retry). After that it reconnects itself.
    pub async fn connect_via(
        provider: Arc<dyn UrlProvider>,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
    ) -> Result<Self, SyncError> {
        let connector = Arc::new(WsBinConnector { url: provider });
        Self::connect_with_tuned(
            connector,
            sink,
            fetcher,
            device_id,
            initial_cursor,
            ChatTuning::default(),
        )
        .await
    }

    pub(crate) async fn connect_with_tuned(
        connector: Arc<dyn BinConnector>,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
        tuning: ChatTuning,
    ) -> Result<Self, SyncError> {
        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (nudge_tx, nudge_rx) = mpsc::channel(1);
        let (probe_tx, probe_rx) = mpsc::channel(1);
        let (redial_tx, redial_rx) = mpsc::channel(1);
        let (presence_tx, presence_rx) = mpsc::channel(4);
        let shared = Arc::new(Mutex::new(Shared {
            cursor: initial_cursor,
            ..Shared::default()
        }));
        let flags = Arc::new(Flags::default());

        let actor = Actor {
            shared: shared.clone(),
            sink,
            fetcher,
            device_id: device_id.to_string(),
            connector,
            tuning,
            events: events.clone(),
            shutdown: shutdown_rx,
            nudge_rx,
            probe_rx,
            redial_rx,
            presence_rx,
            flags: flags.clone(),
            resumed: false,
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                shared,
                events,
                shutdown: shutdown_tx,
                nudge: nudge_tx,
                probe: probe_tx,
                redial: redial_tx,
                presence_out: presence_tx,
                flags,
                task: Some(task),
            }),
            Ok(Err(err)) => {
                task.abort();
                Err(err)
            }
            Err(_) => {
                task.abort();
                Err(SyncError::Closed)
            }
        }
    }

    pub fn events(&self) -> broadcast::Receiver<ChatEvent> {
        self.events.subscribe()
    }

    /// Queue one local update batch for push (a fresh batch id is minted; the
    /// batch survives reconnects until acked — the server dedupes replays).
    ///
    /// Batches over [`MAX_PUSH_BYTES`] are refused here: the server can never
    /// accept them (`MAX_ROW_BYTES`), and a queued-forever batch would replay
    /// on every reconnect — the exact wedge class chat2 replaces. The ops
    /// stay in the local doc and reach peers via the next checkpoint.
    pub fn enqueue_update(&self, bytes: Vec<u8>) {
        if bytes.len() > MAX_PUSH_BYTES {
            use std::sync::atomic::Ordering::Relaxed;
            tracing::error!(
                bytes = bytes.len(),
                "chat2: update exceeds the row cap; not queued (post-strip \
                 updates are KB-scale — this is an upstream bug)"
            );
            self.flags.rejected.fetch_add(1, Relaxed);
            let _ = self.events.send(ChatEvent::PushRejected);
            return;
        }
        {
            let mut shared = lock(&self.shared);
            shared.pending.push_back(PendingPush {
                batch_id: uuid::Uuid::new_v4().to_string(),
                bytes,
            });
        }
        let _ = self.nudge.try_send(());
    }

    /// Publish this device's presence beat with an opaque payload (cursor
    /// positions etc. — relayed verbatim, never stored).
    pub fn send_presence(&self, at: i64, payload: Vec<u8>) {
        let _ = self.presence_out.try_send((at, payload));
    }

    /// Liveness hint: probe the room now (deadline-checked).
    pub fn probe(&self) {
        let _ = self.probe.try_send(());
    }

    /// Escalation: tear the session down and dial a fresh socket.
    pub fn redial(&self) {
        let _ = self.redial.try_send(());
    }

    /// The host posted a checkpoint covering `seq_covered` (C3 policy):
    /// fold it into the cached server view so the thresholds don't re-trip
    /// on stale hello-time numbers every quiesce tick (the DO doesn't
    /// broadcast state after a checkpoint commit).
    pub fn note_checkpoint(&self, seq_covered: u64, size: u64) {
        let mut shared = lock(&self.shared);
        if let Some(server) = &mut shared.server {
            server.checkpoint_seq = seq_covered;
            server.checkpoint_size = size;
            server.seq_floor = seq_covered;
            server.row_count = 0;
            server.row_bytes = 0;
        }
    }

    pub fn stats(&self) -> ChatStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        let shared = lock(&self.shared);
        let server = shared.server.unwrap_or(wire::StateHeader {
            head_seq: 0,
            seq_floor: 0,
            checkpoint_seq: 0,
            checkpoint_size: 0,
            row_count: 0,
            row_bytes: 0,
        });
        ChatStatsSnapshot {
            connected: self.flags.connected.load(Relaxed),
            cursor: shared.cursor,
            // The server's honest view — deliberately NOT clamped to the
            // cursor: cursor > headSeq is the reset signal and must stay
            // visible to the observability surface, not be masked by it.
            head_seq: server.head_seq,
            seq_floor: server.seq_floor,
            checkpoint_seq: server.checkpoint_seq,
            checkpoint_size: server.checkpoint_size,
            row_count: server.row_count,
            row_bytes: server.row_bytes,
            pending_pushes: shared.pending.len() as u64,
            rejoins: self.flags.rejoins.load(Relaxed),
            disconnects: self.flags.disconnects.load(Relaxed),
            rejected: self.flags.rejected.load(Relaxed),
            server_resets: self.flags.server_resets.load(Relaxed),
        }
    }

    /// Leave cleanly and stop the actor.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ChatClient {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// ── the actor ───────────────────────────────────────────────────────────────

struct Actor {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn ChatDocSink>,
    fetcher: Arc<dyn CheckpointFetcher>,
    device_id: String,
    connector: Arc<dyn BinConnector>,
    tuning: ChatTuning,
    events: broadcast::Sender<ChatEvent>,
    shutdown: watch::Receiver<bool>,
    nudge_rx: mpsc::Receiver<()>,
    probe_rx: mpsc::Receiver<()>,
    redial_rx: mpsc::Receiver<()>,
    presence_rx: mpsc::Receiver<(i64, Vec<u8>)>,
    flags: Arc<Flags>,
    /// False until the first backfill of THIS client instance completes.
    /// (Continuity is instance-scoped: a host that restores an older doc
    /// snapshot must construct a fresh `ChatClient` — C3 wiring contract.)
    /// The first
    /// backfill must NOT exclude own rows: after a restart the pending queue
    /// is gone, and a restored-backup/copied-device doc may be missing its
    /// own post-backup writes — they exist only on the server. Loro
    /// re-import of rows the doc does hold is a no-op, so redownloading own
    /// bytes once is pure safety. Same-process reconnects have queue
    /// continuity and skip them (the reconnect-after-offline-work path the
    /// spec optimizes).
    resumed: bool,
}

enum SessionEnd {
    Reconnect,
    Stop,
}

/// How a backoff wait ended.
enum Waited {
    Elapsed,
    /// System wake or a sibling dial succeeded: redial NOW on fresh backoff.
    Woke,
    Shutdown,
}

impl Actor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        // Suspend/resume and sibling-dial successes are EVENTS that end a
        // backoff wait immediately (see room.rs) — without them a recovered
        // network still waited out the full accumulated delay.
        let mut wake = crate::wake::subscribe();
        let mut online = crate::wake::subscribe_online();
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let dial = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect()).await;
            let pipe = match dial {
                Ok(Ok(pipe)) => pipe,
                Ok(Err(err)) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(err));
                        return; // first join failed: caller owns the retry
                    }
                    tracing::warn!(error = %err, "chat2 dial failed; backing off");
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                    continue;
                }
                Err(_) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(SyncError::WebSocket("connect timeout".into())));
                        return;
                    }
                    tracing::warn!("chat2 dial timed out; backing off");
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                    continue;
                }
            };

            match self.run_session(pipe, &mut ready).await {
                SessionEnd::Stop => return,
                SessionEnd::Reconnect => {
                    use std::sync::atomic::Ordering::Relaxed;
                    // A session that had joined resets the backoff — without
                    // this, ~7 flaps pinned every future reconnect at the cap
                    // for the life of the client.
                    let joined = self.flags.connected.swap(false, Relaxed);
                    self.flags.disconnects.fetch_add(1, Relaxed);
                    let _ = self.events.send(ChatEvent::Disconnected);
                    if ready.is_some() {
                        if let Some(ready) = ready.take() {
                            let _ = ready
                                .send(Err(SyncError::Protocol("chat2 handshake failed".into())));
                        }
                        return;
                    }
                    if joined {
                        backoff = BACKOFF_BASE;
                    }
                    match self.wait_backoff(&mut wake, &mut online, backoff).await {
                        Waited::Shutdown => return,
                        Waited::Woke => backoff = BACKOFF_BASE,
                        Waited::Elapsed => backoff = (backoff * 2).min(BACKOFF_CAP),
                    }
                }
            }
        }
    }

    /// Sleep out one backoff, cut short by system wake, a sibling dial
    /// success, or shutdown.
    async fn wait_backoff(
        &mut self,
        wake: &mut tokio::sync::broadcast::Receiver<()>,
        online: &mut tokio::sync::broadcast::Receiver<()>,
        wait: Duration,
    ) -> Waited {
        // Drain stale events: only wakes/successes DURING this wait count,
        // or our own last dial would cut every wait to zero.
        while wake.try_recv().is_ok() {}
        while online.try_recv().is_ok() {}
        tokio::select! {
            _ = tokio::time::sleep(wait) => Waited::Elapsed,
            _ = wake.recv() => Waited::Woke,
            _ = online.recv() => Waited::Woke,
            _ = self.shutdown.changed() => {
                if *self.shutdown.borrow() {
                    Waited::Shutdown
                } else {
                    Waited::Elapsed
                }
            }
        }
    }

    async fn run_session(
        &mut self,
        mut pipe: BinPipe,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> SessionEnd {
        use std::sync::atomic::Ordering::Relaxed;

        // ── hello / state ───────────────────────────────────────────────────
        let cursor = lock(&self.shared).cursor;
        let hello = wire::encode(
            frame_type::HELLO,
            &wire::HelloHeader {
                cursor,
                device: &self.device_id,
            },
            &[],
        );
        if pipe.tx.send(hello).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let state = tokio::time::timeout(HELLO_DEADLINE, async {
            loop {
                let bytes = pipe.rx.recv().await?;
                let Some(frame) = wire::decode(&bytes) else {
                    tracing::warn!("chat2: bad frame during handshake");
                    return None;
                };
                if frame.kind == frame_type::STATE {
                    return Some(frame);
                }
                // Stale broadcast before our state: skip.
            }
        })
        .await;
        let Ok(Some(state_frame)) = state else {
            tracing::warn!("chat2: no state frame within deadline");
            return SessionEnd::Reconnect;
        };
        let Ok(state) = serde_json::from_value::<wire::StateHeader>(state_frame.header.clone())
        else {
            tracing::warn!("chat2: malformed state header");
            return SessionEnd::Reconnect;
        };
        lock(&self.shared).server = Some(state);
        self.flags.connected.store(true, Relaxed);
        if ready.is_none() {
            self.flags.rejoins.fetch_add(1, Relaxed);
        }
        let _ = self.events.send(ChatEvent::Connected);

        // Server behind our cursor = the room was reset/wiped. plan_catch_up
        // treats the cursor as fresh; SURFACE the signal too — the host's
        // re-seed recovery (chat-room.ts /reset) hangs off this event, and
        // masking it was exactly how the s2 wedge class stayed invisible.
        if cursor > state.head_seq {
            self.flags.server_resets.fetch_add(1, Relaxed);
            tracing::warn!(
                cursor,
                head_seq = state.head_seq,
                "chat2: server lost state (headSeq < cursor) — treating as \
                 fresh; host should re-seed via checkpoint"
            );
            let _ = self.events.send(ChatEvent::ServerReset);
        }

        // ── catch-up: checkpoint precision + row backfill ───────────────────
        // Same presence rule as `plan_catch_up`: SIZE, not seq — a seeded
        // room's checkpoint covers seq 0 (see the decision-table test).
        let contained =
            state.checkpoint_size == 0 || self.sink.contains_frontier(&state_frame.payload);
        let plan = plan_catch_up(cursor, &state, contained);
        let after = match plan {
            CatchUpPlan::RowsOnly { after } => after,
            CatchUpPlan::CheckpointThenRows { after } => {
                tracing::info!(
                    checkpoint_seq = state.checkpoint_seq,
                    checkpoint_size = state.checkpoint_size,
                    "chat2: fetching checkpoint"
                );
                // Deadline + shutdown-interruptible: a hung fetch (half-open
                // TCP, stalled link) must neither pin the actor forever nor
                // block `shutdown()`. The fetch is Range-resumable, so the
                // redial retries from wherever the bytes stopped.
                let fetch = self.fetcher.fetch();
                let fetched = tokio::select! {
                    fetched = tokio::time::timeout(CHECKPOINT_FETCH_DEADLINE, fetch) => fetched,
                    _ = self.shutdown.changed() => return SessionEnd::Stop,
                };
                let bytes = match fetched {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "chat2: checkpoint fetch failed");
                        return SessionEnd::Reconnect;
                    }
                    Err(_) => {
                        tracing::warn!("chat2: checkpoint fetch timed out; redialing");
                        return SessionEnd::Reconnect;
                    }
                };
                if let Err(err) = self.sink.apply_checkpoint(&bytes, state.checkpoint_seq) {
                    tracing::warn!(error = %err, "chat2: checkpoint import failed");
                    return SessionEnd::Reconnect;
                }
                let mut shared = lock(&self.shared);
                shared.cursor = shared.cursor.max(state.checkpoint_seq);
                drop(shared);
                let _ = self.events.send(ChatEvent::Applied);
                after
            }
        };
        // Clamp the persisted cursor into the room's honest range (server
        // reset detection happened in plan_catch_up via after==0).
        if after < cursor {
            lock(&self.shared).cursor = after;
        }
        let rows_req = wire::encode(
            frame_type::ROWS_REQ,
            &wire::RowsReqHeader {
                after,
                // First backfill of this process redownloads own rows (see
                // `Actor::resumed`); reconnects skip them.
                exclude_own: self.resumed,
            },
            &[],
        );
        if pipe.tx.send(rows_req).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let backfill = tokio::time::timeout(BACKFILL_DEADLINE, async {
            loop {
                let bytes = pipe.rx.recv().await?;
                let Some(frame) = wire::decode(&bytes) else {
                    return None;
                };
                match frame.kind {
                    frame_type::ROWS_DONE => {
                        let done: wire::RowsDoneHeader =
                            serde_json::from_value(frame.header).ok()?;
                        return Some(done.head_seq);
                    }
                    _ => {
                        if !self.handle_frame(frame) {
                            return None;
                        }
                    }
                }
            }
        })
        .await;
        let Ok(Some(head_seq)) = backfill else {
            tracing::warn!("chat2: backfill did not complete");
            return SessionEnd::Reconnect;
        };
        self.resumed = true;
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let _ = self.events.send(ChatEvent::CaughtUp { head_seq });

        // Anything pending (offline writes, reconnect re-pushes) goes now —
        // the server's batchId dedupe makes replays exact no-ops.
        if !self.push_pending(&mut pipe).await {
            return SessionEnd::Reconnect;
        }

        // ── steady state ────────────────────────────────────────────────────
        let mut last_frame = tokio::time::Instant::now();
        let mut probe_deadline: Option<tokio::time::Instant> = None;
        loop {
            let quiet_probe_at = last_frame + self.tuning.probe_quiet;
            let deadline_at = probe_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            let retry_at = lock(&self.shared)
                .retry_at
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                frame = pipe.rx.recv() => {
                    let Some(bytes) = frame else {
                        return SessionEnd::Reconnect;
                    };
                    last_frame = tokio::time::Instant::now();
                    probe_deadline = None;
                    let Some(frame) = wire::decode(&bytes) else {
                        tracing::warn!("chat2: unparseable frame");
                        return SessionEnd::Reconnect;
                    };
                    if !self.handle_frame(frame) {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.nudge_rx.recv() => {
                    if !self.push_pending(&mut pipe).await {
                        return SessionEnd::Reconnect;
                    }
                }
                beat = self.presence_rx.recv() => {
                    if let Some((at, payload)) = beat {
                        let frame = wire::encode(
                            frame_type::PRESENCE,
                            &wire::PresenceOutHeader { at },
                            &payload,
                        );
                        if pipe.tx.send(frame).await.is_err() {
                            return SessionEnd::Reconnect;
                        }
                    }
                }
                _ = self.probe_rx.recv() => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.redial_rx.recv() => {
                    tracing::info!("chat2: redial requested");
                    return SessionEnd::Reconnect;
                }
                // Transient (quota) rejection: probe with the HEAD batch on
                // a short clock (see `Shared::quota_blocked`); acks re-arm
                // the clock so the queue drains one-per-grant.
                _ = tokio::time::sleep_until(retry_at) => {
                    lock(&self.shared).retry_at = None;
                    if !self.push_head(&mut pipe).await {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = tokio::time::sleep_until(quiet_probe_at) => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                    last_frame = tokio::time::Instant::now();
                }
                _ = tokio::time::sleep_until(deadline_at) => {
                    tracing::warn!("chat2: probe unanswered past deadline; redialing");
                    return SessionEnd::Reconnect;
                }
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        return SessionEnd::Stop;
                    }
                }
            }
        }
    }

    async fn send_probe(
        &self,
        pipe: &mut BinPipe,
        probe_deadline: &mut Option<tokio::time::Instant>,
    ) -> bool {
        let frame = wire::encode(frame_type::PROBE, &serde_json::json!({}), &[]);
        if pipe.tx.send(frame).await.is_err() {
            return false;
        }
        if probe_deadline.is_none() {
            *probe_deadline = Some(tokio::time::Instant::now() + PROBE_DEADLINE);
        }
        true
    }

    /// Send only the queue's head batch — the quota-probe path.
    async fn push_head(&self, pipe: &mut BinPipe) -> bool {
        let frame = {
            let shared = lock(&self.shared);
            shared.pending.front().map(|push| {
                wire::encode(
                    frame_type::PUSH,
                    &wire::PushHeader {
                        batch_id: &push.batch_id,
                    },
                    &push.bytes,
                )
            })
        };
        match frame {
            Some(frame) => pipe.tx.send(frame).await.is_ok(),
            None => true,
        }
    }

    async fn push_pending(&self, pipe: &mut BinPipe) -> bool {
        // Clone rather than drain: batches stay queued until their ack.
        let frames: Vec<Vec<u8>> = lock(&self.shared)
            .pending
            .iter()
            .map(|push| {
                wire::encode(
                    frame_type::PUSH,
                    &wire::PushHeader {
                        batch_id: &push.batch_id,
                    },
                    &push.bytes,
                )
            })
            .collect();
        for frame in frames {
            if pipe.tx.send(frame).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Apply one inbound protocol frame. False = protocol breakdown, redial.
    fn handle_frame(&self, frame: wire::WireFrame) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        match frame.kind {
            frame_type::ROW => {
                let Ok(row) = serde_json::from_value::<wire::RowHeader>(frame.header) else {
                    return false;
                };
                // Own-device rows can still arrive (live relay of a racing
                // second socket, or a server that ignored excludeOwn) — Loro
                // re-import is a no-op; the cursor advance is what matters.
                self.sink.apply_row(&frame.payload, row.seq);
                let mut shared = lock(&self.shared);
                shared.cursor = shared.cursor.max(row.seq);
                drop(shared);
                let _ = self.events.send(ChatEvent::Applied);
            }
            frame_type::ACK => {
                let Ok(ack) = serde_json::from_value::<wire::AckHeader>(frame.header) else {
                    return false;
                };
                let mut shared = lock(&self.shared);
                shared.pending.retain(|p| p.batch_id != ack.batch_id);
                shared.cursor = shared.cursor.max(ack.seq);
                let cursor = shared.cursor;
                // Quota drain: each grant immediately probes the next head
                // batch (one-per-grant, never a full-queue burst).
                if shared.quota_blocked {
                    if shared.pending.is_empty() {
                        shared.quota_blocked = false;
                    } else {
                        shared.retry_at = Some(tokio::time::Instant::now());
                    }
                }
                drop(shared);
                self.sink.advance_cursor(cursor);
                let _ = self.events.send(ChatEvent::Applied);
            }
            frame_type::PRESENCE => {
                let _ = self.events.send(ChatEvent::Presence);
            }
            frame_type::PROBE_OK => {
                if let Ok(probe) = serde_json::from_value::<wire::ProbeOkHeader>(frame.header) {
                    if let Some(server) = &mut lock(&self.shared).server {
                        server.head_seq = server.head_seq.max(probe.head_seq);
                    }
                }
            }
            frame_type::STATE => {
                // Late duplicate of a hello answer — refresh the server view.
                if let Ok(state) = serde_json::from_value::<wire::StateHeader>(frame.header) {
                    lock(&self.shared).server = Some(state);
                }
            }
            frame_type::ERROR => {
                self.flags.rejected.fetch_add(1, Relaxed);
                let code = frame.header["code"].as_str().unwrap_or("?").to_string();
                let message = frame.header["message"].as_str().unwrap_or("").to_string();
                let batch_id = frame.header["batchId"].as_str().unwrap_or("");
                match code.as_str() {
                    // Permanent verdicts on a specific batch: retire it, or
                    // it replays on every nudge/reconnect forever — the
                    // wedge class this design exists to kill. The ops stay
                    // in the local doc and travel with the next checkpoint.
                    "too_large" | "empty" | "bad_push" if !batch_id.is_empty() => {
                        let mut shared = lock(&self.shared);
                        let before = shared.pending.len();
                        shared.pending.retain(|p| p.batch_id != batch_id);
                        let dropped = before != shared.pending.len();
                        drop(shared);
                        if dropped {
                            tracing::error!(
                                code,
                                batch_id,
                                "chat2: batch permanently rejected — retired \
                                 from the replay queue"
                            );
                            let _ = self.events.send(ChatEvent::PushRejected);
                        }
                    }
                    // Transient: the quota window passes on its own — keep
                    // the batch queued and head-probe on a short clock.
                    "quota" => {
                        let mut shared = lock(&self.shared);
                        shared.quota_blocked = true;
                        shared.retry_at = Some(tokio::time::Instant::now() + QUOTA_RETRY);
                    }
                    _ => {}
                }
                tracing::warn!(code, message, "chat2: server rejected a frame");
            }
            other => {
                // Unknown server frame: tolerate (future protocol additions).
                tracing::debug!(kind = other, "chat2: ignoring unknown frame type");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
