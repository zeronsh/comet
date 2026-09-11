use super::*;
use crate::{RpcReply, RpcService};
use async_trait::async_trait;
use std::{sync::atomic::AtomicUsize, time::Duration};

async fn nonreading_peer() -> (
    RpcClient,
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(64 * 1024).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let accept = async {
        let (socket, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(socket).await.unwrap()
    };
    let (client, peer) = tokio::join!(connect_ws(&url), accept);
    (client.unwrap(), peer)
}

async fn back_up_real_socket(client: &RpcClient) {
    // Larger than TCP's send/receive buffers. The peer completed its handshake
    // but never polls reads, so this pins the actual pump inside sink.send.
    let large = serde_json::to_string(&ClientFrame {
        id: 1_000_000,
        method: Some("Never".into()),
        params: serde_json::json!("x".repeat(8 * 1024 * 1024)),
        cancel: false,
    })
    .unwrap();
    client.out.send(large).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut sent = 0;
        loop {
            let text = format!("{{\"id\":{},\"method\":\"Never\"}}", 1_000_001 + sent);
            match client.out.try_send(text) {
                Ok(()) => {
                    sent += 1;
                    assert!(sent <= 1024, "real socket must backpressure");
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if client.out.capacity() == 0 {
                        break;
                    }
                }
                Err(_) => panic!("socket must still be live before cancellation"),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("nonreading peer must saturate actual socket and bounded queues");
    assert!(!client.transport.as_ref().unwrap().is_finished());
}

async fn assert_pump_stopped_before_peer_reads(
    pump: tokio::task::AbortHandle,
    mut peer: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !pump.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual socket pump must terminate while peer remains nonreading");
    // Only after proving pump termination, drain already-buffered TCP data and
    // require EOF/reset: task completion must also release the actual socket.
    use tokio::io::AsyncReadExt;
    let socket = peer.get_mut();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = [0; 64 * 1024];
        let mut total = 0;
        loop {
            match socket.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    assert!(total <= 16 * 1024 * 1024);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("unexpected socket error: {error}"),
            }
        }
    })
    .await
    .expect("actual TCP connection must close after buffered bytes drain");
}

#[tokio::test]
async fn nonreading_peer_pump_terminates_on_client_drop() {
    let (client, peer) = nonreading_peer().await;
    back_up_real_socket(&client).await;
    let pump = client.transport.as_ref().unwrap().abort_handle();
    drop(client);
    assert_pump_stopped_before_peer_reads(pump, peer).await;
}

#[tokio::test]
async fn nonreading_peer_pump_terminates_on_cancellation_overflow() {
    let (client, peer) = nonreading_peer().await;
    let mut call = Box::pin(client.call("Never", serde_json::Value::Null));
    assert!(futures::poll!(&mut call).is_pending());
    back_up_real_socket(&client).await;
    let pump = client.transport.as_ref().unwrap().abort_handle();
    drop(call); // the real unary guard cannot enqueue cancel into the full queue
    assert!(*client.shared.shutdown.borrow());
    assert_pump_stopped_before_peer_reads(pump, peer).await;
    assert!(client.shared.lock().is_empty());
    assert!(matches!(
        client.call("Never", serde_json::Value::Null).await,
        Err(RpcError::Closed)
    ));
}

#[tokio::test]
async fn cancellation_pressure_closes_transport_instead_of_spawning_backlog() {
    let (out, mut peer) = mpsc::channel(1);
    let (_remote, inbound) = mpsc::channel(1);
    let client = RpcClient::new(out, inbound);
    let mut calls = Vec::new();
    for _ in 0..(STREAM_QUEUE_CAP + 8) {
        let mut call = Box::pin(client.call("Never", serde_json::Value::Null));
        assert!(futures::poll!(&mut call).is_pending());
        calls.push(call);
    }
    // Tokio's cooperative poll budget may yield before filling 256 slots.
    // Drive the pending calls until actual writer saturation, not a poll count.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            for call in &mut calls {
                assert!(futures::poll!(call).is_pending());
            }
            if client.out.capacity() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("must fill bounded writer while peer is stalled");
    assert!(client.out.capacity() == 0);
    drop(calls);
    assert!(client.shared.lock().is_empty());
    tokio::time::timeout(Duration::from_secs(1), async {
        while peer.recv().await.is_some() {}
        while !client.reader.is_finished() || !client.writer.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded cancellation overload must close actual transport");
    assert!(matches!(
        client.call("Never", serde_json::Value::Null).await,
        Err(RpcError::Closed)
    ));
    assert!(client.shared.lock().is_empty());
}

#[tokio::test]
async fn completed_unary_does_not_emit_cancel() {
    let (out, mut peer) = mpsc::channel(4);
    let (remote, inbound) = mpsc::channel(4);
    let client = RpcClient::new(out, inbound);
    let mut call = Box::pin(client.call("Echo", serde_json::Value::Null));
    let request = tokio::select! { _ = &mut call => panic!("no reply yet"), request = peer.recv() => request.unwrap() };
    let request: ClientFrame = serde_json::from_str(&request).unwrap();
    remote
        .send(
            serde_json::to_string(&ServerFrame {
                id: request.id,
                ok: Some(serde_json::json!(true)),
                ..Default::default()
            })
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(call.await.unwrap(), true);
    assert!(client.shared.lock().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), peer.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_unary_releases_pending_before_reply() {
    let (out, mut peer) = mpsc::channel(4);
    let (_remote, inbound) = mpsc::channel(4);
    let client = RpcClient::new(out, inbound);
    let mut call = Box::pin(client.call("Never", serde_json::Value::Null));
    let frame = tokio::select! { biased; _ = &mut call => panic!("must remain pending"), frame = peer.recv() => frame.unwrap() };
    let id = serde_json::from_str::<ClientFrame>(&frame).unwrap().id;
    assert_eq!(client.shared.lock().len(), 1);
    drop(call);
    assert!(
        client.shared.lock().is_empty(),
        "cancelled unary must not accumulate pending entries"
    );
    let cancel = tokio::time::timeout(Duration::from_secs(1), peer.recv())
        .await
        .unwrap()
        .unwrap();
    let cancel: ClientFrame = serde_json::from_str(&cancel).unwrap();
    assert!(cancel.cancel);
    assert_eq!(cancel.id, id);
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct NeverService {
    active: Arc<AtomicUsize>,
    started: tokio::sync::Notify,
}
#[async_trait]
impl RpcService for NeverService {
    async fn handle(&self, method: &str, _: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == "Echo" {
            return RpcReply::value(&true);
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let _guard = Active(self.active.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn repeated_network_unary_cancellation_drops_server_tasks() {
    let service = Arc::new(NeverService {
        active: Arc::new(AtomicUsize::new(0)),
        started: tokio::sync::Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(crate::serve_ws_listener(listener, service.clone()));
    let client = connect_ws(&url).await.unwrap();
    for _ in 0..64 {
        let mut call = Box::pin(client.call("Never", serde_json::Value::Null));
        tokio::select! { _ = &mut call => panic!("must stay pending"), _ = service.started.notified() => {} }
        drop(call);
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping call must cancel native server work");
        assert!(client.shared.lock().is_empty());
    }
    assert_eq!(
        client.call("Echo", serde_json::Value::Null).await.unwrap(),
        true
    );
    server.abort();
}

#[tokio::test]
async fn custom_transport_does_not_flush_buffered_work_after_close() {
    let (transport_out, mut outbound) = mpsc::channel(1);
    let (_inbound_tx, inbound) = mpsc::channel(1);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (sent_tx, mut sent_rx) = mpsc::channel(1);
    let client = RpcClient::new_with_transport(transport_out, inbound, async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        while let Some(frame) = outbound.recv().await {
            let _ = sent_tx.send(frame).await;
        }
    });
    started_rx.await.unwrap();
    client
        .out
        .send(serde_json::json!({"method":"Mutation"}).to_string())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while client.out.capacity() != STREAM_QUEUE_CAP {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer must hand buffered work to the custom transport");

    client.close();
    let _ = release_tx.send(());
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(50), sent_rx.recv()).await,
            Ok(Some(_))
        ),
        "shutdown must win before a custom pump flushes buffered work"
    );
}
