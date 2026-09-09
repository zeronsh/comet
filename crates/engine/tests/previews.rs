use futures::StreamExt;
use std::{sync::Arc, time::Duration};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::{HarnessId, PreviewService};
use zeron_rpc::{RpcReply, RpcService, methods};

fn service(device: &str, cwd: &std::path::Path, id: &str) -> PreviewService {
    PreviewService {
        id: id.into(),
        project_id: "project".into(),
        project_name: "project".into(),
        project_cwd: cwd.to_string_lossy().into_owned(),
        device_id: device.into(),
        device_name: device.into(),
        hostname: format!("{device}.{id}.localhost"),
        name: "Vite".into(),
        port: 5173,
        pid: 123,
        cwd: cwd.to_string_lossy().into_owned(),
        started_at: 1,
        zeron_owned: true,
    }
}
#[tokio::test]
async fn preview_watch_follows_the_session_checkout_and_owning_device() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let core = EngineCore::assemble(
        &temp.path().join("engine"),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    core.workspace
        .create_chat(
            "chat",
            None,
            Some("peer"),
            None,
            Some(a.to_string_lossy().into_owned()),
        )
        .unwrap();
    let catalog = core.previews.catalog();
    catalog
        .set_remote(
            "peer",
            vec![service("peer", &a, "a"), service("peer", &b, "b")],
        )
        .unwrap();
    catalog
        .set_remote("other", vec![service("other", &a, "other")])
        .unwrap();
    let rpc = core.rpc_service();
    let RpcReply::Stream(mut stream) = rpc
        .handle(
            methods::WATCH_PREVIEWS,
            serde_json::json!({"chatId":"chat", "targetDeviceId":"peer"}),
        )
        .await
        .unwrap()
    else {
        panic!("expected preview watch")
    };
    let first = stream.next().await.unwrap();
    assert_eq!(first["services"].as_array().unwrap().len(), 1);
    assert_eq!(first["services"][0]["id"], "a");
    assert_eq!(first["remote"], true);
    core.workspace
        .set_chat_cwd("chat", &b.to_string_lossy())
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next["services"][0]["id"], "b");
    catalog.remove_remote("peer");
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next["services"], serde_json::json!([]));
    drop(stream);
    core.shutdown().await;
}
