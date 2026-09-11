//! Client side: request/stream multiplexing over string frames + the WebSocket dialer.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(feature = "native")]
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
#[cfg(feature = "native")]
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{ClientFrame, RpcError, ServerFrame};

mod task;
#[cfg(all(test, feature = "native"))]
mod tests;

/// Per-stream queue depth. Bounded: route_frame awaits a full queue, pausing
/// the connection reader — transport backpressure instead of unbounded growth
/// when a consumer stalls behind a fast producer (watch frames every 120ms
/// during streaming used to pile up whole-transcript payloads here).
const STREAM_QUEUE_CAP: usize = 256;

enum Pending {
    Call(oneshot::Sender<Result<serde_json::Value, RpcError>>),
    Stream(mpsc::Sender<serde_json::Value>),
    CheckedStream {
        items: mpsc::Sender<serde_json::Value>,
        ready: Option<oneshot::Sender<Result<(), RpcError>>>,
    },
}

struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
    shutdown: watch::Sender<bool>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancel(&self, id: u64, sent: bool, out: &mpsc::Sender<String>) {
        if self.lock().remove(&id).is_none() || !sent {
            return;
        }
        let frame = serde_json::to_string(&ClientFrame {
            id,
            method: None,
            params: serde_json::Value::Null,
            cancel: true,
        })
        .expect("cancel envelope is serializable");
        if out.try_send(frame).is_err() {
            // No detached send tasks/unbounded cancellation backlog. If the
            // bounded writer cannot accept cancellation, close this transport;
            // dropping its outbound channel cancels all server-side requests.
            self.shutdown.send_replace(true);
        }
    }
}

/// Owns a pending entry from insertion (including while send is blocked) until
/// reply/stream completion or cancellation. Cancellation never retries a call,
/// nor does it prove a mutation was not applied before the cancel arrived.
struct PendingGuard {
    id: Option<u64>,
    sent: bool,
    out: mpsc::Sender<String>,
    shared: Arc<Shared>,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.shared.cancel(id, self.sent, &self.out);
        }
    }
}

/// A multiplexing RPC client over string frames. One bounded writer owns the
/// actual transport sender, so overload can close both memory and WS transports.
pub struct RpcClient {
    out: mpsc::Sender<String>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    reader: task::Task,
    writer: task::Task,
    transport: Option<task::Task>,
}

/// Checked stream receiver whose drop immediately cancels the server task.
pub struct RpcSubscription {
    items: mpsc::Receiver<serde_json::Value>,
    _pending: PendingGuard,
}
impl RpcSubscription {
    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        self.items.recv().await
    }
}

impl RpcClient {
    /// Close this viewport's transport and fail pending work. No remote engine
    /// command is sent, and no call is retried. Idempotent across shared handles.
    pub fn close(&self) {
        self.shared.shutdown.send_replace(true);
    }

    /// Subscribe to closure of this viewport's transport. The notification is
    /// edge-triggered for callers that need to replace their application state;
    /// it carries no retry or delivery guarantee for in-flight mutations.
    pub fn watch_closed(&self) -> watch::Receiver<bool> {
        self.shared.shutdown.subscribe()
    }

    /// Wrap an existing duplex: `out` carries client frames, `inbound` server frames.
    pub fn new(transport_out: mpsc::Sender<String>, mut inbound: mpsc::Receiver<String>) -> Self {
        let (shutdown, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            shutdown,
        });
        let (out, mut outgoing) = mpsc::channel::<String>(STREAM_QUEUE_CAP);
        let writer_shared = shared.clone();
        let mut writer_closed = shared.shutdown.subscribe();
        let writer = task::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = writer_closed.wait_for(|closed| *closed) => break,
                    _ = transport_out.closed() => break,
                    frame = outgoing.recv() => match frame { Some(frame) => frame, None => break },
                };
                tokio::select! {
                    biased;
                    _ = writer_closed.wait_for(|closed| *closed) => break,
                    sent = transport_out.send(frame) => if sent.is_err() { break; },
                }
            }
            writer_shared.shutdown.send_replace(true);
            // transport_out drops here, closing the actual transport even if
            // callers still hold clones of the bounded local out sender.
        });
        let reader_shared = shared.clone();
        let reader_out = out.clone();
        let mut reader_closed = shared.shutdown.subscribe();
        let reader = task::spawn(async move {
            'read: loop {
                let payload = tokio::select! {
                    biased;
                    _ = reader_closed.wait_for(|closed| *closed) => break,
                    payload = inbound.recv() => match payload { Some(payload) => payload, None => break },
                };
                for line in payload.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame = match crate::decode_server_frame(line) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::warn!(error = %err, "rpc: dropping malformed server frame");
                            continue;
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = reader_closed.wait_for(|closed| *closed) => break 'read,
                        _ = route_frame(&reader_shared, &reader_out, frame) => {},
                    }
                }
            }
            reader_shared.shutdown.send_replace(true);
            // Connection closed: fail everything still pending.
            let drained: Vec<Pending> = {
                let mut pending = reader_shared.lock();
                pending.drain().map(|(_, p)| p).collect()
            };
            for entry in drained {
                match entry {
                    Pending::Call(tx) => {
                        let _ = tx.send(Err(RpcError::Closed));
                    }
                    Pending::CheckedStream {
                        ready: Some(ready), ..
                    } => {
                        let _ = ready.send(Err(RpcError::Closed));
                    }
                    Pending::Stream(_) | Pending::CheckedStream { ready: None, .. } => {}
                }
                // Stream item receivers end by sender drop.
            }
        });
        Self {
            out,
            shared,
            next_id: AtomicU64::new(1),
            reader,
            writer,
            transport: None,
        }
    }

    /// Wrap a duplex and retain a caller-provided transport pump for the
    /// client's lifetime. Browser adapters use this to keep WebSocket callbacks
    /// alive after handing the typed client to application state.
    ///
    /// The pump must stop when `transport_out` is dropped and must drop its
    /// inbound sender when the underlying transport closes. Neither condition
    /// retries calls or buffers writes for a later connection.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_with_transport<F>(
        transport_out: mpsc::Sender<String>,
        inbound: mpsc::Receiver<String>,
        transport: F,
    ) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut client = Self::new(transport_out, inbound);
        let mut closed = client.shared.shutdown.subscribe();
        client.transport = Some(task::spawn(async move {
            tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => {},
                _ = transport => {},
            }
        }));
        client
    }

    /// WASM transport pumps run on the browser's local executor and therefore
    /// intentionally do not require `Send`.
    #[cfg(target_arch = "wasm32")]
    pub fn new_with_transport<F>(
        transport_out: mpsc::Sender<String>,
        inbound: mpsc::Receiver<String>,
        transport: F,
    ) -> Self
    where
        F: Future<Output = ()> + 'static,
    {
        let mut client = Self::new(transport_out, inbound);
        let mut closed = client.shared.shutdown.subscribe();
        client.transport = Some(task::spawn(async move {
            tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => {},
                _ = transport => {},
            }
        }));
        client
    }

    /// Unary request.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.lock().insert(id, Pending::Call(tx));
        let mut pending = self.pending_guard(id);
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        pending.sent = true;
        rx.await.map_err(|_| RpcError::Closed)?
    }

    /// Typed unary request.
    pub async fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, RpcError> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| RpcError::BadParams(e.to_string()))
    }

    /// Streaming request: items arrive on the receiver; it closes when the server sends
    /// `{done}` or `{err}`, or the connection drops. Dropping the receiver cancels the
    /// stream server-side (the reader notices the dead channel and sends `{id, cancel}`).
    pub async fn subscribe(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<mpsc::Receiver<serde_json::Value>, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(STREAM_QUEUE_CAP);
        self.shared.lock().insert(id, Pending::Stream(tx));
        let mut pending = self.pending_guard(id);
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        // Legacy receiver cancellation stays reader-driven after setup.
        pending.id = None;
        Ok(rx)
    }

    /// Streaming request with a server acknowledgement before returning.
    ///
    /// Use this for optional/versioned stream methods: an older server's
    /// `unknown method` is returned distinctly instead of looking like an empty stream.
    pub async fn subscribe_checked(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcSubscription, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (items_tx, items_rx) = mpsc::channel(STREAM_QUEUE_CAP);
        let (ready_tx, ready_rx) = oneshot::channel();
        self.shared.lock().insert(
            id,
            Pending::CheckedStream {
                items: items_tx,
                ready: Some(ready_tx),
            },
        );
        let mut pending = self.pending_guard(id);
        self.send(ClientFrame {
            id,
            method: Some(method.into()),
            params,
            cancel: false,
        })
        .await
        .inspect_err(|_| {
            self.shared.lock().remove(&id);
        })?;
        pending.sent = true;
        let subscription = RpcSubscription {
            items: items_rx,
            _pending: pending,
        };
        ready_rx.await.map_err(|_| RpcError::Closed)??;
        Ok(subscription)
    }

    fn pending_guard(&self, id: u64) -> PendingGuard {
        PendingGuard {
            id: Some(id),
            sent: false,
            out: self.out.clone(),
            shared: self.shared.clone(),
        }
    }

    async fn send(&self, frame: ClientFrame) -> Result<(), RpcError> {
        let json = serde_json::to_string(&frame)
            .map_err(|e| RpcError::Transport(format!("serialize frame: {e}")))?;
        let mut closed = self.shared.shutdown.subscribe();
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => Err(RpcError::Closed),
            result = self.out.send(json) => result.map_err(|_| RpcError::Closed),
        }
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.shared.shutdown.send_replace(true);
        self.writer.abort();
        if let Some(transport) = &self.transport {
            transport.abort();
        }
        self.reader.abort();
    }
}

async fn route_frame(shared: &Arc<Shared>, out: &mpsc::Sender<String>, frame: ServerFrame) {
    let id = frame.id;
    if let Some(err) = frame.err {
        let error = wire_error(err);
        match shared.lock().remove(&id) {
            Some(Pending::Call(tx)) => {
                let _ = tx.send(Err(error));
            }
            Some(Pending::CheckedStream {
                ready: Some(ready), ..
            }) => {
                let _ = ready.send(Err(error));
            }
            Some(Pending::Stream(_)) | Some(Pending::CheckedStream { ready: None, .. }) | None => {
                // Stream errored: the sender drop closes the receiver.
                tracing::debug!(id, %error, "rpc: stream ended with error");
            }
        }
        return;
    }
    if let Some(value) = frame.ok {
        let mut pending = shared.lock();
        if matches!(pending.get(&id), Some(Pending::Call(_))) {
            if let Some(Pending::Call(tx)) = pending.remove(&id) {
                let _ = tx.send(Ok(value));
            }
        } else if let Some(Pending::CheckedStream { ready, .. }) = pending.get_mut(&id)
            && let Some(ready) = ready.take()
        {
            let _ = ready.send(Ok(()));
        }
        return;
    }
    if let Some(item) = frame.item {
        let ready = match shared.lock().get_mut(&id) {
            Some(Pending::CheckedStream { ready, .. }) => ready.take(),
            _ => None,
        };
        if let Some(ready) = ready {
            let _ = ready.send(Ok(()));
        }
        // Clone the sender out of the lock: the bounded send must await
        // (backpressure) without holding `shared`.
        let tx = match shared.lock().get(&id) {
            Some(Pending::Stream(tx)) => Some(tx.clone()),
            Some(Pending::CheckedStream { items, .. }) => Some(items.clone()),
            _ => None,
        };
        let dead = match tx {
            Some(tx) => tx.send(item).await.is_err(),
            None => false,
        };
        if dead {
            // Receiver was dropped — cancel server-side and forget the stream.
            shared.lock().remove(&id);
            if let Ok(json) = serde_json::to_string(&ClientFrame {
                id,
                method: None,
                params: serde_json::Value::Null,
                cancel: true,
            }) {
                let _ = out.send(json).await;
            }
        }
        return;
    }
    if frame.done
        && let Some(Pending::CheckedStream {
            ready: Some(ready), ..
        }) = shared.lock().remove(&id)
    {
        let _ = ready.send(Ok(()));
    }
}

fn wire_error(error: String) -> RpcError {
    error
        .strip_prefix("unknown method: ")
        .map(|method| RpcError::UnknownMethod(method.to_owned()))
        .unwrap_or(RpcError::Failed(error))
}

/// How long a dial may take before we give up.
///
/// This is localhost: a real engine answers in milliseconds. Without a bound,
/// *any* other process holding the port accepts the TCP connection and then
/// never completes the WebSocket handshake, and the caller waits forever — a
/// stranger on port 27654 would hang the app at boot rather than degrade it.
#[cfg(feature = "native")]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Dial a WebSocket RPC server (`ws://127.0.0.1:{ipc_port}`).
#[cfg(feature = "native")]
pub async fn connect_ws(url: &str) -> Result<RpcClient, RpcError> {
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| RpcError::Transport(format!("timed out dialing {url}")))?
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let mut client = RpcClient::new(out_tx, in_rx);
    let mut closed = client.shared.shutdown.subscribe();
    client.transport = Some(task::spawn(async move {
        let pump = async move {
            loop {
                tokio::select! {
                    frame = out_rx.recv() => match frame {
                        Some(text) => {
                            if sink.send(WsMessage::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            let _ = sink.send(WsMessage::Close(None)).await;
                            break;
                        }
                    },
                    message = stream.next() => match message {
                        Some(Ok(WsMessage::Text(text))) => {
                            if in_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
        };
        // Race the entire pump, not only its outer receive loop: sink.send,
        // close-frame flushing and inbound channel backpressure can all wait
        // indefinitely. Shutdown must drop the actual socket from any await.
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => {},
            _ = pump => {},
        }
    }));
    Ok(client)
}
