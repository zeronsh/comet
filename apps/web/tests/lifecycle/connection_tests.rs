use crate::engine_connection::{EngineHandle, EngineMode};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::{
    task::LocalSet,
    time::{Duration, timeout},
};
use zeron_proto::{EngineInfo, WorkspaceScope};
use zeron_rpc::{ClientFrame, RpcClient, RpcError, methods};

use crate::browser_connection::ConnectionEpochs;
use crate::browser_session::LifecycleCoordinator;
fn transport() -> (RpcClient, mpsc::Receiver<String>, mpsc::Sender<String>) {
    let (out, requests) = mpsc::channel(4);
    let (responses, inbound) = mpsc::channel(4);
    (RpcClient::new(out, inbound), requests, responses)
}

async fn request(requests: &mut mpsc::Receiver<String>, method: &str) -> ClientFrame {
    let frame: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
    assert_eq!(frame.method.as_deref(), Some(method));
    assert!(!frame.cancel);
    frame
}

async fn reply(responses: &mpsc::Sender<String>, id: u64, value: Value) {
    responses
        .send(json!({"id": id, "ok": value}).to_string())
        .await
        .unwrap();
}

fn info() -> EngineInfo {
    EngineInfo {
        device_id: "test-device".into(),
        workspace_scope: WorkspaceScope::Local,
        capabilities: Vec::new(),
    }
}

#[tokio::test]
async fn connected_handle_requires_identity_and_readiness_and_only_closes_viewport() {
    let (client, mut requests, responses) = transport();
    let attach = EngineHandle::from_connected_client(client, "ws://test/api/rpc".into());
    let peer = async {
        let identity = request(&mut requests, methods::ENGINE_INFO).await;
        reply(
            &responses,
            identity.id,
            serde_json::to_value(info()).unwrap(),
        )
        .await;
        let ready = request(&mut requests, methods::ENGINE_READY).await;
        reply(&responses, ready.id, json!({"ready": true})).await;
    };
    let (handle, ()) = tokio::join!(attach, peer);
    let handle = handle.unwrap();
    assert_eq!(handle.engine_info().device_id, "test-device");
    assert_eq!(handle.engine_info().workspace_scope, WorkspaceScope::Local);
    assert_eq!(
        handle.mode(),
        EngineMode::Remote {
            url: "ws://test/api/rpc".into()
        }
    );
    assert!(
        requests.try_recv().is_err(),
        "attachment must not start watches or mutations"
    );
    let clone = handle.clone();
    handle.shutdown().await;
    // The server receives no StopEngine (or other command), only EOF.
    assert!(requests.recv().await.is_none());
    responses.closed().await;
    assert!(matches!(
        clone.client().call("AfterClose", json!({})).await,
        Err(RpcError::Closed)
    ));
}

#[tokio::test]
async fn connected_handle_rejects_not_ready_without_starting_watches() {
    let (client, mut requests, responses) = transport();
    let attach = EngineHandle::from_connected_client(client, "ws://test/api/rpc".into());
    let peer = async {
        let identity = request(&mut requests, methods::ENGINE_INFO).await;
        reply(
            &responses,
            identity.id,
            serde_json::to_value(info()).unwrap(),
        )
        .await;
        let ready = request(&mut requests, methods::ENGINE_READY).await;
        reply(&responses, ready.id, json!({"ready": false})).await;
    };
    let (result, ()) = tokio::join!(attach, peer);
    assert!(matches!(result, Err(RpcError::Failed(_))));
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn dropping_attachment_during_readiness_releases_transport() {
    let (client, mut requests, responses) = transport();
    let mut attach = Box::pin(EngineHandle::from_connected_client(
        client,
        "ws://test/api/rpc".into(),
    ));
    let peer = async {
        let identity = request(&mut requests, methods::ENGINE_INFO).await;
        reply(
            &responses,
            identity.id,
            serde_json::to_value(info()).unwrap(),
        )
        .await;
        request(&mut requests, methods::ENGINE_READY).await;
    };
    tokio::select! {
        _ = &mut attach => panic!("attachment completed before readiness"),
        () = peer => {},
    }
    drop(attach);
    // Cancellation may already be queued; no other RPC may escape.
    while let Some(text) = requests.recv().await {
        let frame: ClientFrame = serde_json::from_str(&text).unwrap();
        assert!(frame.cancel);
    }
    responses.closed().await;
}

#[test]
fn auth_and_socket_epochs_reject_stale_attachment_results() {
    let mut epochs = ConnectionEpochs::default();
    let signed_in = epochs.begin_auth();
    assert!(epochs.is_current(signed_in));

    let first_socket = epochs.begin_socket();
    assert!(epochs.is_current(first_socket));
    assert!(!epochs.is_current(signed_in));

    let reconnect = epochs.begin_socket();
    assert!(epochs.is_current(reconnect));
    assert!(!epochs.is_current(first_socket));

    let signed_out = epochs.begin_auth();
    assert!(epochs.is_current(signed_out));
    assert!(!epochs.is_current(reconnect));
}

#[test]
fn request_epoch_cancels_outstanding_requests_and_rejects_late_starts() {
    let mut coordinator = LifecycleCoordinator::default();
    let (first_epoch, cancelled) = coordinator.begin_epoch();
    assert!(cancelled.is_empty());
    let first = coordinator.begin_request(first_epoch).unwrap();
    let second = coordinator.begin_request(first_epoch).unwrap();

    let (next_epoch, cancelled) = coordinator.begin_epoch();
    assert_eq!(cancelled, vec![first, second]);
    assert!(coordinator.begin_request(first_epoch).is_none());
    let current = coordinator.begin_request(next_epoch).unwrap();
    coordinator.finish_request(current);
    let (_, cancelled) = coordinator.begin_epoch();
    assert!(cancelled.is_empty());
}

#[test]
fn activity_requires_real_input_and_is_throttled() {
    let mut coordinator = LifecycleCoordinator::default();
    const INTERVAL_MS: f64 = 60_000.0;

    // Calling this represents a keyboard or pointer event; idle time never does.
    assert!(coordinator.should_report_activity(1_000.0, INTERVAL_MS));
    assert!(!coordinator.should_report_activity(1_001.0, INTERVAL_MS));
    assert!(!coordinator.should_report_activity(60_999.0, INTERVAL_MS));
    assert!(coordinator.should_report_activity(61_000.0, INTERVAL_MS));
}

struct TestSocket;

impl crate::browser_connection::SocketSink for TestSocket {
    fn send(&self, _: &str) -> Result<(), ()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pump_keeps_a_popped_frame_across_an_ordinary_socket_notification() {
    LocalSet::new()
        .run_until(async {
            use crate::browser_connection::{Inbox, Signal, SocketState, pump, signal};
            use std::{cell::RefCell, rc::Rc};

            let inbox = Rc::new(RefCell::new(Inbox::default()));
            assert!(inbox.borrow_mut().push("payload".into()));
            let (signal_tx, signal_rx) = tokio::sync::watch::channel(Signal {
                state: SocketState::Open,
                sequence: 0,
            });
            let (_outbound_tx, outbound_rx) = mpsc::channel(1);
            let (inbound_tx, mut inbound_rx) = mpsc::channel(1);
            inbound_tx.send("occupied".into()).await.unwrap();

            let task = tokio::task::spawn_local(pump(
                TestSocket,
                inbox,
                signal_rx,
                outbound_rx,
                inbound_tx,
            ));
            tokio::task::yield_now().await;
            signal(&signal_tx, SocketState::Open);
            tokio::task::yield_now().await;

            assert_eq!(inbound_rx.recv().await.as_deref(), Some("occupied"));
            assert_eq!(
                timeout(Duration::from_millis(100), inbound_rx.recv())
                    .await
                    .expect("ordinary open notification must not discard a queued frame")
                    .as_deref(),
                Some("payload")
            );
            signal(&signal_tx, SocketState::Closed);
            task.await.unwrap();
        })
        .await;
}
