//! Exercise the client-only feature through its real channel transport. No server,
//! engine, native WebSocket dialer or application state is needed.
#![cfg(all(feature = "client", not(target_arch = "wasm32")))]

use serde_json::json;
use tokio::sync::mpsc;
use zeron_rpc::{ClientFrame, RpcClient, RpcError, ServerFrame, decode_server_frame};

#[tokio::test]
async fn typed_call_uses_the_shared_wire_protocol() {
    let (out, mut requests) = mpsc::channel(1);
    let (responses, inbound) = mpsc::channel(1);
    let client = RpcClient::new(out, inbound);
    let call = client.call_as::<Vec<String>>("List", json!({"scope": "one"}));
    let peer = async {
        let frame: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        assert_eq!(frame.method.as_deref(), Some("List"));
        assert_eq!(frame.params, json!({"scope": "one"}));
        assert!(!frame.cancel);
        responses
            .send(json!({"id": frame.id, "ok": ["first", "second"]}).to_string())
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(call, peer);
    assert_eq!(result.unwrap(), ["first", "second"]);
}

#[tokio::test]
async fn checked_subscription_drop_cancels_without_another_item() {
    let (out, mut requests) = mpsc::channel(1);
    let (responses, inbound) = mpsc::channel(1);
    let client = RpcClient::new(out, inbound);
    let subscription = client.subscribe_checked("Watch", json!({}));
    let peer = async {
        let frame: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        responses
            .send(json!({"id": frame.id, "ok": {}}).to_string())
            .await
            .unwrap();
        frame.id
    };
    let (subscription, id) = tokio::join!(subscription, peer);
    drop(subscription.unwrap());
    let cancel: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
    assert_eq!(cancel.id, id);
    assert!(cancel.cancel);
    assert!(cancel.method.is_none());
}

#[tokio::test]
async fn disconnect_fails_pending_call_and_closes_transport() {
    let (out, mut requests) = mpsc::channel(1);
    let (responses, inbound) = mpsc::channel(1);
    let client = RpcClient::new(out, inbound);
    let call = client.call("Never", json!({}));
    let peer = async {
        requests.recv().await.unwrap();
        drop(responses);
        assert!(requests.recv().await.is_none());
    };
    let (result, ()) = tokio::join!(call, peer);
    assert!(matches!(result, Err(RpcError::Closed)));
}

#[tokio::test]
async fn dropping_client_releases_both_transport_channels() {
    let (out, mut requests) = mpsc::channel(1);
    let (responses, inbound) = mpsc::channel(1);
    let client = RpcClient::new(out, inbound);
    drop(client);
    assert!(requests.recv().await.is_none());
    responses.closed().await;
}

#[tokio::test]
async fn retained_transport_pump_forwards_and_reports_disconnect() {
    let (transport_out, mut outbound) = mpsc::channel(1);
    let (inbound_tx, inbound) = mpsc::channel(1);
    let client = RpcClient::new_with_transport(transport_out, inbound, async move {
        while let Some(request) = outbound.recv().await {
            let request: ClientFrame = serde_json::from_str(&request).unwrap();
            inbound_tx
                .send(json!({"id": request.id, "ok": "through-pump"}).to_string())
                .await
                .unwrap();
        }
    });
    let mut closed = client.watch_closed();
    assert_eq!(
        client.call("Echo", json!({})).await.unwrap(),
        "through-pump"
    );
    client.close();
    closed.changed().await.unwrap();
    assert!(*closed.borrow());
}

#[tokio::test]
async fn null_server_values_survive_canonical_parse_and_client_routing() {
    assert_eq!(
        decode_server_frame(r#"{"id":1,"ok":null}"#).unwrap().ok,
        Some(serde_json::Value::Null)
    );
    assert_eq!(
        serde_json::from_str::<ServerFrame>(r#"{"id":2,"item":null}"#)
            .unwrap()
            .item,
        Some(serde_json::Value::Null)
    );

    let (out, mut requests) = mpsc::channel(2);
    let (responses, inbound) = mpsc::channel(2);
    let client = RpcClient::new(out, inbound);
    let call = client.call("Null", json!({}));
    let peer = async {
        let request: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        responses
            .send(json!({"id": request.id, "ok": null}).to_string())
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(call, peer);
    assert_eq!(result.unwrap(), serde_json::Value::Null);

    let subscription = client.subscribe("WatchNull", json!({}));
    let peer = async {
        let request: ClientFrame = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
        responses
            .send(json!({"id": request.id, "item": null}).to_string())
            .await
            .unwrap();
        responses
            .send(json!({"id": request.id, "done": true}).to_string())
            .await
            .unwrap();
    };
    let (subscription, ()) = tokio::join!(subscription, peer);
    let mut subscription = subscription.unwrap();
    assert_eq!(subscription.recv().await, Some(serde_json::Value::Null));
    assert_eq!(subscription.recv().await, None);
}
