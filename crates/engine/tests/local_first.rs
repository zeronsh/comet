//! Local-first startup boundaries and captured synced-session behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};
use zeron_engine::{AuthState, Engine, EngineConfig, EngineInfo, HarnessId, WorkspaceScope};
use zeron_rpc::{connect_ws, memory_client, methods};

fn config(
    data_dir: &std::path::Path,
    edge_url: String,
    workos_client_id: Option<&str>,
    edge_token: Option<&str>,
) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        edge_url,
        edge_token: edge_token.map(str::to_string),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        workos_client_id: workos_client_id.map(str::to_string),
    }
}

async fn rejecting_edge() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                seen.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"revoked"}"#;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), requests, task)
}

struct DaemonEdge {
    url: String,
    active: Arc<Mutex<HashMap<String, usize>>>,
    task: tokio::task::JoinHandle<()>,
}

impl DaemonEdge {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let active = Arc::new(Mutex::new(HashMap::new()));
        let active_for_task = active.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let active = active_for_task.clone();
                tokio::spawn(async move { serve_daemon_edge(stream, active).await });
            }
        });
        Self { url, active, task }
    }

    fn active_matching(&self, prefix: &str) -> usize {
        self.active
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(_, count)| *count)
            .sum()
    }

    fn active_total(&self) -> usize {
        self.active.lock().unwrap().values().sum()
    }
}

impl Drop for DaemonEdge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ActiveSocket {
    active: Arc<Mutex<HashMap<String, usize>>>,
    path: String,
}

impl Drop for ActiveSocket {
    fn drop(&mut self) {
        let mut active = self.active.lock().unwrap();
        if let Some(count) = active.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.path);
            }
        }
    }
}

async fn serve_daemon_edge(
    mut stream: tokio::net::TcpStream,
    active: Arc<Mutex<HashMap<String, usize>>>,
) {
    let mut peeked = [0u8; 8192];
    let request_len = loop {
        let Ok(read) = stream.peek(&mut peeked).await else {
            return;
        };
        if read == 0 {
            return;
        }
        if peeked[..read].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break read;
        }
        tokio::task::yield_now().await;
    };
    let headers = String::from_utf8_lossy(&peeked[..request_len]);
    if !headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("upgrade: websocket"))
    {
        let mut request = [0u8; 8192];
        let Ok(read) = stream.read(&mut request).await else {
            return;
        };
        let request = String::from_utf8_lossy(&request[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let body = if target.starts_with("/auth/refresh") {
            let claims = serde_json::json!({
                "exp": 4_102_444_800_u64,
                "org_id": "org_1"
            });
            let claims =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            serde_json::json!({
                "accessToken": format!("e30.{claims}.sig"),
                "refreshToken": "rotated-refresh"
            })
            .to_string()
        } else {
            r#"{"releases":[]}"#.to_string()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    let path = Arc::new(Mutex::new(String::new()));
    let captured = path.clone();
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &WsRequest, response: WsResponse| {
            *captured.lock().unwrap() = request.uri().path().to_string();
            Ok(response)
        },
    )
    .await
    else {
        return;
    };
    let path = path.lock().unwrap().clone();
    *active.lock().unwrap().entry(path.clone()).or_default() += 1;
    let _active = ActiveSocket {
        active,
        path: path.clone(),
    };
    let (mut sink, mut source) = ws.split();
    while let Some(frame) = source.next().await {
        let Ok(frame) = frame else { return };
        if path.starts_with("/registry/")
            && let WsMessage::Text(text) = frame
        {
            if text == "ping" {
                if sink.send(WsMessage::Text("pong".into())).await.is_err() {
                    return;
                }
            } else if serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|frame| {
                    frame
                        .get("t")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .as_deref()
                == Some("hello")
            {
                let state = serde_json::json!({
                    "t": "state",
                    "seq": 0,
                    "full": true,
                    "gcFloor": 0,
                    "rows": [],
                    "presence": {}
                });
                if sink
                    .send(WsMessage::Text(state.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn wait_until(mut check: impl FnMut() -> bool, message: &str) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{message}");
}

#[tokio::test]
async fn signed_out_workos_boot_serves_local_data_without_dev_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready without auth");

    assert_eq!(scope, WorkspaceScope::Local);
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    let client = memory_client(runtime.core().rpc_service());
    let info: EngineInfo = client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(info.workspace_scope, WorkspaceScope::Local);
    assert!(
        client
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap()
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(dir.path().join("profiles/local").is_dir());
    assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
    assert!(runtime.core().links().is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn clean_local_auth_construction_does_not_probe_edge_health() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);

    let auth = Engine::build_auth(&config).await;

    assert_eq!(
        Engine::initial_workspace_scope(&auth),
        WorkspaceScope::Local
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    edge_task.abort();
}

#[tokio::test]
async fn local_runtime_does_not_start_the_edge_updater() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(
        runtime.core().updater().is_none(),
        "local runtime must not start an Edge updater"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn revoked_captured_session_stays_on_its_synced_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"dead","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("persisted org resolves before refresh");

    assert!(auth.loaded_workos_session());
    assert_eq!(scope, WorkspaceScope::Synced);
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    // Revocation is detected by the background session probe — assembly no
    // longer blocks on the network round trip — so the SignedOut transition
    // lands shortly after assembly, not during it.
    for _ in 0..200 {
        if auth.state() == AuthState::SignedOut {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.loaded_workos_session());
    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Synced);
    assert!(dir.path().join("orgs/org_1/user_1").is_dir());
    assert!(!dir.path().join("profiles/local").exists());
    assert!(requests.load(Ordering::SeqCst) >= 1);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn transient_refresh_failure_keeps_synced_recovery_supervisors_alive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("persisted org resolves while Edge is unavailable");

    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Synced);
    assert!(
        auth.state().is_signed_in(),
        "network errors are not revocation"
    );
    assert!(
        runtime.core().links().is_some(),
        "peer routing must recover without restarting the app"
    );
    assert!(
        runtime.core().updater().is_some(),
        "the Edge updater supervisor must survive an offline boot"
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn development_without_an_explicit_bearer_stays_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, None, None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("development profile is ready");

    assert_eq!(scope, WorkspaceScope::Development);
    assert_eq!(auth.access_token().await.as_deref(), Some("dev-user"));
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(
        runtime.core().links().is_none(),
        "the synthetic dev identity must not enable Edge"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn explicit_dev_bearer_keeps_online_routing_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        None,
        Some("dev-user@dev-org"),
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("development profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(runtime.core().links().is_some());
    assert!(dir.path().join("orgs/dev-org/dev-user").is_dir());
    runtime.shutdown().await;
}

#[tokio::test]
async fn headless_stop_rpc_drains_the_daemon_and_releases_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");

    assert_eq!(
        client
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .unwrap(),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop")
        .expect("headless task panicked")
        .expect("headless shutdown failed");

    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after shutdown");
}

#[tokio::test]
async fn headless_sign_out_closes_joined_edge_rooms_and_stops_daemon() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let edge = DaemonEdge::start().await;
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(dir.path(), edge.url.clone(), Some("client_test"), None);
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");
    wait_until(
        || edge.active_matching("/registry/") > 0,
        "registry room did not connect",
    )
    .await;
    wait_until(
        || edge.active_matching("/device/") > 0,
        "device relay did not connect",
    )
    .await;

    assert_eq!(
        client
            .call(methods::SIGN_OUT, serde_json::json!({}))
            .await
            .expect("SignOut reply must reach the caller before IPC closes"),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop after sign-out")
        .expect("headless task panicked")
        .expect("headless shutdown failed");
    wait_until(
        || edge.active_total() == 0,
        "authenticated Edge sockets survived daemon sign-out",
    )
    .await;

    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after sign-out");
}

/// Replacing a synced/online runtime must be a real ownership boundary: after
/// `shutdown()` returns, no worker may send another Edge request (updater,
/// room joins), and dropping the runtime must actually free
/// the engine graph — the sessions ⇄ doc-host cycle and the strong-`self`
/// worker loops previously kept a replaced runtime alive and polling forever.
#[tokio::test]
async fn online_runtime_shutdown_stops_edge_workers_and_retires_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, None, Some("dev-user@dev-org"));
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("development profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    let retired = runtime.core().doc_host.retirement_probe();

    // Live traffic proof: the woken release checker (and the room joins) must
    // be hitting the counting edge before the boundary is exercised.
    if let Some(updater) = runtime.core().updater() {
        updater.check_now();
    }
    wait_until(
        || requests.load(Ordering::SeqCst) >= 2,
        "edge workers never produced traffic before shutdown",
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(30), runtime.shutdown())
        .await
        .expect("shutdown never returned — an Edge worker did not join");
    let after = requests.load(Ordering::SeqCst);
    drop(runtime);

    wait_until(
        &*retired,
        "engine graph still reachable after shutdown + drop",
    )
    .await;
    // Long enough to cover a couple of worker retry periods: a surviving
    // Edge worker loop would land another request in this window.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        after,
        "edge received requests after shutdown returned"
    );
    edge_task.abort();
}
