//! HostRelay + ClientLink end-to-end over an in-memory fake device room.
//!
//! The fake implements the `DeviceRoom` DO's relay semantics (edge/src/device-room.ts):
//! route client frames to the single host socket with `from` stamped; route host frames
//! by `to` (bounce `client_gone` when the target left); host supersede (a new host join
//! closes the predecessor); `client_closed` on client disconnect; `host_closed` broadcast
//! on host disconnect; `host_offline` bounce when a client sends with no host; nudge
//! frames delivered to the host.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a full
// `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use zeron_crypto::channel::{ChannelIdentity, ChannelScope, PeerIdentity};
use zeron_rpc::device_room::{
    CLIENT_CLOSED, CLIENT_GONE, HOST_CLOSED, HOST_OFFLINE, NUDGE_KIND, RELAY_KIND,
};
use zeron_rpc::{
    ChannelAuthority, ChannelHost, ChannelLocal, DeviceFrameHeader, DeviceLink, HostRelay,
    HostRelayConfig, LinkCache, LinkCacheConfig, RpcError, RpcReply, RpcService, StaticToken,
    TokenSource, decode_device_frame, device_room_ws_url, encode_device_frame, methods,
};

// ---------------------------------------------------------------------------
// Fake relay (the DO semantics, in-memory)
// ---------------------------------------------------------------------------

enum Out {
    Frame(Vec<u8>),
    Close,
}

#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Out>>,
    clients: HashMap<String, mpsc::UnboundedSender<Out>>,
    /// Zombie-path simulation: the host stays "connected" (no bounce) but
    /// client→host frames vanish — the 2026-08-19 dead edge↔host leg.
    blackhole_host_bound: bool,
    /// Plaintext `rpc` frames the relay saw in either direction — the
    /// leakage canary for the device-channel tests.
    plaintext_rpc_frames: usize,
    /// Every distinct frame kind the relay routed.
    kinds_seen: Vec<String>,
}

struct FakeRelay {
    port: u16,
    state: Arc<Mutex<RelayState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeRelay {
    async fn start() -> FakeRelay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(Mutex::new(RelayState::default()));
        let accept_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle_socket(stream, accept_state.clone()));
            }
        });
        FakeRelay { port, state, task }
    }

    fn edge_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn host_connected(&self) -> bool {
        self.state.lock().expect("lock").host.is_some()
    }

    async fn wait_host_connected(&self) {
        wait_until(|| self.host_connected()).await;
    }

    /// Swallow client→host frames while keeping the host registered — the
    /// zombie relay path (client↔edge healthy, edge↔host dead).
    fn set_blackhole_host_bound(&self, on: bool) {
        self.state.lock().expect("lock").blackhole_host_bound = on;
    }

    fn plaintext_rpc_frames(&self) -> usize {
        self.state.lock().expect("lock").plaintext_rpc_frames
    }

    fn kinds_seen(&self) -> Vec<String> {
        self.state.lock().expect("lock").kinds_seen.clone()
    }

    /// Deliver a nudge frame to the connected host (the DO's /nudge live path).
    fn nudge(&self, chat_id: &str) {
        let header = DeviceFrameHeader::new(chat_id, NUDGE_KIND);
        let payload = serde_json::json!({ "chatId": chat_id }).to_string();
        let frame = encode_device_frame(&header, payload.as_bytes()).expect("encode nudge");
        let state = self.state.lock().expect("lock");
        state
            .host
            .as_ref()
            .expect("host connected")
            .send(Out::Frame(frame))
            .expect("send");
    }
}

fn relay_error(code: &str) -> Vec<u8> {
    serde_json::json!({ "error": code })
        .to_string()
        .into_bytes()
}

async fn handle_socket(stream: tokio::net::TcpStream, state: Arc<Mutex<RelayState>>) {
    let mut uri = String::new();
    let ws =
        match tokio_tungstenite::accept_hdr_async(stream, |req: &WsRequest, res: WsResponse| {
            uri = req.uri().to_string();
            Ok(res)
        })
        .await
        {
            Ok(ws) => ws,
            Err(_) => return,
        };
    let query: HashMap<String, String> = uri
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let is_host = query.get("role").map(String::as_str) == Some("host");
    let conn_id = query
        .get("connId")
        .cloned()
        .unwrap_or_else(|| "anon".into());

    let (mut sink, mut ws_stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    {
        let mut st = state.lock().expect("lock");
        if is_host {
            // One live host socket: close any predecessor (backend restart / supersede).
            if let Some(old) = st.host.take() {
                let _ = old.send(Out::Close);
            }
            st.host = Some(tx.clone());
        } else {
            st.clients.insert(conn_id.clone(), tx.clone());
        }
    }

    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Frame(bytes) => {
                    if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                Out::Close => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            }
        }
    });

    while let Some(message) = ws_stream.next().await {
        let bytes = match message {
            Ok(WsMessage::Binary(bytes)) => bytes,
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok((header, payload)) = decode_device_frame(&bytes) else {
            break;
        };
        let mut st = state.lock().expect("lock");
        if header.k == "rpc" {
            st.plaintext_rpc_frames += 1;
        }
        if !st.kinds_seen.contains(&header.k) {
            st.kinds_seen.push(header.k.clone());
        }
        if !is_host {
            if st.blackhole_host_bound {
                continue; // zombie path: frame vanishes, no bounce
            }
            match &st.host {
                Some(host) => {
                    let mut routed = DeviceFrameHeader::new(header.s, header.k);
                    routed.from = Some(conn_id.clone());
                    let _ = host.send(Out::Frame(
                        encode_device_frame(&routed, &payload).expect("encode"),
                    ));
                }
                None => {
                    let bounce = DeviceFrameHeader::new(header.s, RELAY_KIND);
                    let _ = tx.send(Out::Frame(
                        encode_device_frame(&bounce, &relay_error(HOST_OFFLINE)).expect("encode"),
                    ));
                }
            }
            continue;
        }
        // Host frame: route by `to`.
        let Some(to) = header.to else { continue };
        match st.clients.get(&to) {
            Some(client) => {
                let stripped = DeviceFrameHeader::new(header.s, header.k);
                let _ = client.send(Out::Frame(
                    encode_device_frame(&stripped, &payload).expect("encode"),
                ));
            }
            None => {
                let bounce = DeviceFrameHeader::new(header.s, RELAY_KIND).with_to(to);
                let _ = tx.send(Out::Frame(
                    encode_device_frame(&bounce, &relay_error(CLIENT_GONE)).expect("encode"),
                ));
            }
        }
    }

    // Disconnect bookkeeping (mirrors DeviceRoom.webSocketClose).
    {
        let mut st = state.lock().expect("lock");
        if is_host {
            if st.host.as_ref().is_some_and(|h| h.same_channel(&tx)) {
                st.host = None;
            }
            for client in st.clients.values() {
                let header = DeviceFrameHeader::new("", RELAY_KIND);
                let _ = client.send(Out::Frame(
                    encode_device_frame(&header, &relay_error(HOST_CLOSED)).expect("encode"),
                ));
            }
        } else {
            st.clients.remove(&conn_id);
            if let Some(host) = &st.host {
                let mut header = DeviceFrameHeader::new("", RELAY_KIND);
                header.from = Some(conn_id.clone());
                let _ = host.send(Out::Frame(
                    encode_device_frame(&header, &relay_error(CLIENT_CLOSED)).expect("encode"),
                ));
            }
        }
    }
    writer.abort();
}

// ---------------------------------------------------------------------------
// Test service + helpers
// ---------------------------------------------------------------------------

struct TestService {
    label: String,
    active_streams: Arc<AtomicUsize>,
}

impl TestService {
    fn new(label: &str) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            active_streams: Arc::new(AtomicUsize::new(0)),
        })
    }
}

struct StreamGuard(Arc<AtomicUsize>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl RpcService for TestService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::LIST_HARNESSES => Ok(RpcReply::Value(serde_json::json!([]))),
            "Echo" => Ok(RpcReply::Value(
                serde_json::json!({ "host": self.label, "params": params }),
            )),
            "Count" => {
                let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(RpcReply::Stream(
                    futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                ))
            }
            "Never" => {
                self.active_streams.fetch_add(1, Ordering::SeqCst);
                let guard = StreamGuard(self.active_streams.clone());
                Ok(RpcReply::Stream(
                    futures::stream::poll_fn(move |_| {
                        let _keep = &guard;
                        std::task::Poll::Pending
                    })
                    .boxed(),
                ))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached within 5s");
}

fn relay_config(edge_url: &str, retry_ms: u64) -> HostRelayConfig {
    let mut config =
        HostRelayConfig::new(edge_url, "dev-a", Arc::new(StaticToken("test-user".into())));
    config.retry = Duration::from_millis(retry_ms);
    config
}

fn cache(edge_url: &str) -> Arc<LinkCache> {
    let mut config = LinkCacheConfig::new(edge_url, Arc::new(StaticToken("test-user".into())));
    config.cooldown_base = Duration::from_millis(100);
    config.cooldown_max = Duration::from_millis(400);
    config.probe_timeout = Duration::from_millis(1_500);
    LinkCache::new(config)
}

fn noop_nudge() -> zeron_rpc::NudgeHandler {
    Arc::new(|_| {})
}

// ---------------------------------------------------------------------------
// Device-channel membership authority (the engine's vault, in miniature)
// ---------------------------------------------------------------------------

const SCOPE: ChannelScope = ChannelScope {
    vault_id: [7; 16],
    generation: [8; 16],
};

/// One device's channel identity plus the members it currently trusts.
struct MemberAuthority {
    device_id: [u8; 16],
    secret: [u8; 32],
    members: Mutex<Vec<PeerIdentity>>,
    required: bool,
}

impl MemberAuthority {
    fn new(tag: u8, required: bool) -> Arc<Self> {
        Arc::new(Self {
            device_id: [tag; 16],
            secret: [tag ^ 0xa5; 32],
            members: Mutex::new(Vec::new()),
            required,
        })
    }

    fn identity(&self) -> PeerIdentity {
        PeerIdentity {
            device_id: self.device_id,
            static_key: ChannelIdentity::new(self.device_id, &self.secret)
                .expect("identity")
                .public_key(),
        }
    }

    fn trust(&self, other: &MemberAuthority) {
        self.members.lock().expect("lock").push(other.identity());
    }

    fn revoke(&self, other: &MemberAuthority) {
        let gone = other.identity();
        self.members.lock().expect("lock").retain(|m| *m != gone);
    }
}

impl ChannelAuthority for MemberAuthority {
    fn required(&self) -> bool {
        self.required
    }

    fn local(&self) -> Result<ChannelLocal, String> {
        Ok(ChannelLocal {
            identity: ChannelIdentity::new(self.device_id, &self.secret)
                .map_err(|e| e.to_string())?,
            scope: SCOPE,
        })
    }

    fn accept(&self, peer: &PeerIdentity) -> bool {
        self.members.lock().expect("lock").contains(peer)
    }
}

fn secured_host(
    edge_url: &str,
    authority: Arc<MemberAuthority>,
    service: Arc<TestService>,
) -> HostRelay {
    let mut config = relay_config(edge_url, 100);
    config.channel = Some(ChannelHost {
        authority,
        service: service.clone(),
    });
    HostRelay::spawn(config, service, noop_nudge())
}

fn secured_cache(edge_url: &str, authority: Arc<MemberAuthority>) -> Arc<LinkCache> {
    let mut config = LinkCacheConfig::new(edge_url, Arc::new(StaticToken("test-user".into())));
    config.cooldown_base = Duration::from_millis(100);
    config.cooldown_max = Duration::from_millis(400);
    config.probe_timeout = Duration::from_millis(1_500);
    config.channel = Some(authority);
    LinkCache::new(config)
}

struct RecoveringToken {
    value: Mutex<Option<String>>,
    changes: tokio::sync::watch::Sender<u64>,
}

impl RecoveringToken {
    fn new(value: Option<&str>) -> Self {
        let (changes, _) = tokio::sync::watch::channel(0);
        Self {
            value: Mutex::new(value.map(str::to_string)),
            changes,
        }
    }

    fn replace(&self, value: &str) {
        *self.value.lock().expect("lock") = Some(value.into());
        self.changes
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn clear(&self) {
        *self.value.lock().expect("lock") = None;
        self.changes
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

#[async_trait]
impl TokenSource for RecoveringToken {
    async fn token(&self) -> Option<String> {
        self.value.lock().expect("lock").clone()
    }

    fn subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.changes.subscribe())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_recovery_wakes_a_signed_out_host_relay_immediately() {
    let relay = FakeRelay::start().await;
    let token = Arc::new(RecoveringToken::new(None));
    let mut config = HostRelayConfig::new(relay.edge_url(), "dev-a", token.clone());
    // Without the token signal this test would remain asleep for two minutes.
    config.retry = Duration::from_secs(120);
    let _host = HostRelay::spawn(config, TestService::new("host-a"), noop_nudge());
    tokio::task::yield_now().await;
    assert!(!relay.host_connected());

    token.replace("test-user");

    relay.wait_host_connected().await;
}

#[tokio::test]
async fn sign_out_closes_a_live_host_relay_immediately() {
    let relay = FakeRelay::start().await;
    let token = Arc::new(RecoveringToken::new(Some("test-user")));
    let mut config = HostRelayConfig::new(relay.edge_url(), "dev-a", token.clone());
    // A sign-out signal must close the authenticated socket, not wait for retry.
    config.retry = Duration::from_secs(120);
    let _host = HostRelay::spawn(config, TestService::new("host-a"), noop_nudge());
    relay.wait_host_connected().await;

    token.clear();

    wait_until(|| !relay.host_connected()).await;
}

#[tokio::test]
async fn token_rotation_does_not_interrupt_a_live_peer_link() {
    let relay = FakeRelay::start().await;
    let _host = HostRelay::spawn(
        relay_config(&relay.edge_url(), 100),
        TestService::new("host-a"),
        noop_nudge(),
    );
    relay.wait_host_connected().await;

    let token = Arc::new(RecoveringToken::new(Some("old-token")));
    let links = LinkCache::new(LinkCacheConfig::new(relay.edge_url(), token.clone()));
    let client = links.client("dev-a").await.expect("client dials");
    token.replace("new-token");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let echoed = client
        .call("Echo", serde_json::json!({ "after": "refresh" }))
        .await
        .expect("live link survives token rotation");
    assert_eq!(echoed["params"]["after"], "refresh");
}

#[tokio::test]
async fn sign_out_closes_cached_peer_links() {
    let relay = FakeRelay::start().await;
    let _host = HostRelay::spawn(
        relay_config(&relay.edge_url(), 100),
        TestService::new("host-a"),
        noop_nudge(),
    );
    relay.wait_host_connected().await;

    let token = Arc::new(RecoveringToken::new(Some("test-user")));
    let links = LinkCache::new(LinkCacheConfig::new(relay.edge_url(), token.clone()));
    let client = links.client("dev-a").await.expect("client dials");
    client
        .call("Echo", serde_json::json!({ "before": "sign-out" }))
        .await
        .expect("link is live before sign-out");

    for _ in 0..32 {
        zeron_sync::wake::notify_online();
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.clear();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client
                .call("Echo", serde_json::json!({ "after": "sign-out" }))
                .await
                .is_err()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cached peer link survived sign-out");
    assert!(
        links.client("dev-a").await.is_err(),
        "signed-out cache must not redial"
    );
}

#[tokio::test]
async fn relay_serves_multiple_clients_end_to_end() {
    let relay = FakeRelay::start().await;
    let service = TestService::new("host-a");
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;

    let links = cache(&relay.edge_url());
    let a = links.client("dev-a").await.expect("client a dials");
    let b = links.client("dev-a").await.expect("client b reuses/dials");

    let echoed = a
        .call("Echo", serde_json::json!({ "who": "a" }))
        .await
        .expect("echo a");
    assert_eq!(echoed["host"], "host-a");
    assert_eq!(echoed["params"]["who"], "a");

    // Streaming through the relay: items arrive in order, stream terminates.
    let mut items = b
        .subscribe("Count", serde_json::json!({ "n": 3 }))
        .await
        .expect("count");
    let mut seen = Vec::new();
    while let Some(v) = items.recv().await {
        seen.push(v);
    }
    assert_eq!(
        seen,
        vec![
            serde_json::json!(0),
            serde_json::json!(1),
            serde_json::json!(2)
        ]
    );

    // Concurrent calls from the same cached link multiplex fine.
    let (x, y) = tokio::join!(
        a.call("Echo", serde_json::json!(1)),
        a.call("Echo", serde_json::json!(2))
    );
    assert_eq!(x.expect("x")["params"], serde_json::json!(1));
    assert_eq!(y.expect("y")["params"], serde_json::json!(2));
}

#[tokio::test]
async fn client_disconnect_tears_down_virtual_conn() {
    let relay = FakeRelay::start().await;
    let service = TestService::new("host-a");
    let active = service.active_streams.clone();
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;

    let url = device_room_ws_url(&relay.edge_url(), "dev-a", "client", Some("conn-x"), "t");
    let link = DeviceLink::connect(&url).await.expect("link connects");
    let client = link.client();
    let _items = client
        .subscribe("Never", serde_json::Value::Null)
        .await
        .expect("subscribe");
    wait_until(|| active.load(Ordering::SeqCst) == 1).await;

    // Dropping the link closes the relay socket → the DO tells the host client_closed →
    // the host drops the virtual conn, aborting the client's server-side streams.
    drop(link);
    wait_until(|| active.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn host_offline_fails_fast_and_cools_down() {
    let relay = FakeRelay::start().await;
    let links = cache(&relay.edge_url());

    // No host connected: the readiness probe is bounced with host_offline → link-down →
    // the dial fails quickly instead of hanging.
    let Err(err) = links.client("dev-a").await else {
        panic!("dial must fail with no host")
    };
    let message = err.to_string();
    assert!(
        message.contains("readiness check"),
        "expected readiness failure, got: {message}"
    );

    // Immediately after, the cooldown makes callers fail fast without redialing.
    let Err(err) = links.client("dev-a").await else {
        panic!("must fail fast while cooling")
    };
    assert!(err.to_string().contains("backing off"), "got: {err}");

    // After the cooldown a host is up — dial succeeds and clears the slate.
    let service = TestService::new("host-a");
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let client = links.client("dev-a").await.expect("dials after cooldown");
    assert_eq!(
        client
            .call("Echo", serde_json::json!({}))
            .await
            .expect("echo")["host"],
        "host-a"
    );
}

/// The data-driven cooldown reset (fresh workspace presence → peer is alive):
/// with a long backoff engaged, `reset_cooldown` lets the next call dial
/// immediately instead of waiting the window out.
#[tokio::test]
async fn presence_reset_clears_cooldown_immediately() {
    let relay = FakeRelay::start().await;
    let mut config =
        LinkCacheConfig::new(relay.edge_url(), Arc::new(StaticToken("test-user".into())));
    // Long enough that only a reset (never elapsed time) can explain success.
    config.cooldown_base = Duration::from_secs(120);
    config.cooldown_max = Duration::from_secs(120);
    config.probe_timeout = Duration::from_millis(1_500);
    let links = LinkCache::new(config);

    // No host: the dial fails and the two-minute cooldown engages.
    assert!(links.client("dev-a").await.is_err(), "no host: dial fails");
    let Err(err) = links.client("dev-a").await else {
        panic!("must fail fast while cooling")
    };
    assert!(err.to_string().contains("backing off"), "got: {err}");

    // Host comes up and its presence heartbeat clears the backoff — the next
    // call dials immediately.
    let service = TestService::new("host-a");
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;
    links.reset_cooldown("dev-a");
    let client = links.client("dev-a").await.expect("dials after reset");
    assert_eq!(
        client
            .call("Echo", serde_json::json!({}))
            .await
            .expect("echo")["host"],
        "host-a"
    );
}

#[tokio::test]
async fn host_supersede_drops_old_links_and_recovers() {
    let relay = FakeRelay::start().await;
    let service = TestService::new("host-a");
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;

    let links = cache(&relay.edge_url());
    let client = links.client("dev-a").await.expect("dials");
    assert!(client.call("Echo", serde_json::json!({})).await.is_ok());

    // A rogue host joins: the relay closes the HostRelay's socket (supersede) and, when
    // the rogue's socket later closes, clients see host_closed. The HostRelay backs off
    // and reclaims the room; old links are down and the cache re-dials.
    let rogue_url = device_room_ws_url(&relay.edge_url(), "dev-a", "host", None, "t");
    let (rogue_ws, _) = tokio_tungstenite::connect_async(&rogue_url)
        .await
        .expect("rogue joins");
    // Wait for the supersede to land, then let the rogue die: the HostRelay's reconnect
    // supersedes it right back (proved by its socket closing).
    let (_, mut rogue_stream) = rogue_ws.split();
    loop {
        match rogue_stream.next().await {
            Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
            Some(Ok(_)) => {}
        }
    }
    relay.wait_host_connected().await;

    // The pre-supersede link died; in-flight/new calls on it fail rather than hang.
    let err = client
        .call("Echo", serde_json::json!({}))
        .await
        .expect_err("old link dead");
    assert!(
        matches!(err, RpcError::Closed | RpcError::Transport(_)),
        "got: {err}"
    );

    // The cache notices the dead link and re-dials the reclaimed host (allowing for a
    // cooldown window if a re-dial raced the reclaim).
    let recovered = loop {
        match links.client("dev-a").await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(120)).await,
        }
    };
    let echoed = recovered
        .call("Echo", serde_json::json!({}))
        .await
        .expect("echo");
    assert_eq!(echoed["host"], "host-a");
}

#[tokio::test]
async fn nudges_reach_the_host_callback() {
    let relay = FakeRelay::start().await;
    let service = TestService::new("host-a");
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let on_nudge: zeron_rpc::NudgeHandler = Arc::new(move |chat_id| {
        let _ = tx.send(chat_id);
    });
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, on_nudge);
    relay.wait_host_connected().await;

    relay.nudge("chat-42");
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("nudge delivered")
        .expect("channel open");
    assert_eq!(got, "chat-42");
}

// ---------------------------------------------------------------------------
// Authenticated device channel (RFC 0001 §10)
// ---------------------------------------------------------------------------

/// Two mutually-trusting members: RPC and streams work end to end, and the
/// relay never sees a plaintext `rpc` frame — only handshake/ciphertext
/// (`chan`) and payload-less echoes.
#[tokio::test]
async fn device_channel_carries_rpc_with_no_plaintext_on_the_relay() {
    let relay = FakeRelay::start().await;
    let host_authority = MemberAuthority::new(0x11, true);
    let client_authority = MemberAuthority::new(0x22, true);
    host_authority.trust(&client_authority);
    client_authority.trust(&host_authority);
    let _host = secured_host(
        &relay.edge_url(),
        host_authority.clone(),
        TestService::new("host-a"),
    );
    relay.wait_host_connected().await;

    let links = secured_cache(&relay.edge_url(), client_authority.clone());
    let client = links.client("dev-a").await.expect("secured dial");
    let echoed = client
        .call("Echo", serde_json::json!({ "secret": "plaintext canary" }))
        .await
        .expect("echo over the channel");
    assert_eq!(echoed["host"], "host-a");
    assert_eq!(echoed["params"]["secret"], "plaintext canary");

    let mut items = client
        .subscribe("Count", serde_json::json!({ "n": 3 }))
        .await
        .expect("stream over the channel");
    let mut seen = Vec::new();
    while let Some(v) = items.recv().await {
        seen.push(v.as_u64().expect("number"));
    }
    assert_eq!(seen, vec![0, 1, 2]);

    // A frame past Noise's 64 KiB message cap is chunked and reassembled.
    let big = "x".repeat(200_000);
    let echoed = client
        .call("Echo", serde_json::json!({ "big": big }))
        .await
        .expect("large echo");
    assert_eq!(
        echoed["params"]["big"].as_str().map(str::len),
        Some(200_000)
    );

    assert_eq!(
        relay.plaintext_rpc_frames(),
        0,
        "no plaintext RPC crossed the relay"
    );
    let kinds = relay.kinds_seen();
    assert!(kinds.contains(&"chan".to_string()), "kinds: {kinds:?}");
    assert!(
        kinds.iter().all(|k| k == "chan" || k == "echo"),
        "unexpected frame kinds on the relay: {kinds:?}"
    );

    // The link authenticated the host's vault identity, not a relay id.
    let url = device_room_ws_url(&relay.edge_url(), "dev-a", "client", Some("c-peer"), "t");
    let link = DeviceLink::connect_secured(&url, client_authority.as_ref())
        .await
        .expect("direct secured link");
    assert_eq!(link.peer(), Some(host_authority.identity()));
}

/// An enrolled host refuses plaintext: a legacy client's very first RPC
/// (the readiness probe) is answered with `encrypted_channel_required`
/// and the dial fails — no downgrade, no cached link.
#[tokio::test]
async fn enrolled_host_refuses_plaintext_clients() {
    let relay = FakeRelay::start().await;
    let host_authority = MemberAuthority::new(0x11, true);
    let _host = secured_host(
        &relay.edge_url(),
        host_authority,
        TestService::new("host-a"),
    );
    relay.wait_host_connected().await;

    let links = cache(&relay.edge_url());
    let Err(err) = links.client("dev-a").await else {
        panic!("plaintext dial must be refused");
    };
    assert!(
        err.to_string().contains("encrypted_channel_required"),
        "got: {err}"
    );
}

/// Membership gates both directions: a host that does not list the caller
/// refuses it after message 3, and a caller that does not list the host
/// refuses BEFORE revealing its own identity (message 3 never leaves).
#[tokio::test]
async fn device_channel_refuses_non_members_on_either_side() {
    let relay = FakeRelay::start().await;
    let host_authority = MemberAuthority::new(0x11, true);
    let member = MemberAuthority::new(0x22, true);
    let stranger = MemberAuthority::new(0x33, true);
    host_authority.trust(&member);
    member.trust(&host_authority);
    stranger.trust(&host_authority);
    let _host = secured_host(
        &relay.edge_url(),
        host_authority.clone(),
        TestService::new("host-a"),
    );
    relay.wait_host_connected().await;

    // Stranger trusts the host, but the host does not list the stranger.
    let url = device_room_ws_url(
        &relay.edge_url(),
        "dev-a",
        "client",
        Some("c-stranger"),
        "t",
    );
    let link = DeviceLink::connect_secured(&url, stranger.as_ref()).await;
    let refused = match link {
        Err(err) => err.to_string(),
        Ok(link) => {
            // The handshake may complete on the client before the host's
            // verdict arrives; the first call then surfaces the rejection.
            let outcome = link.client().call("Echo", serde_json::json!({})).await;
            outcome
                .expect_err("stranger must not get an answer")
                .to_string()
        }
    };
    assert!(
        refused.contains("channel_rejected")
            || refused.contains("Closed")
            || refused.contains("closed"),
        "got: {refused}"
    );

    // A member that no longer trusts the host: refused locally, before message 3.
    member.revoke(&host_authority);
    let url = device_room_ws_url(&relay.edge_url(), "dev-a", "client", Some("c-member"), "t");
    let Err(err) = DeviceLink::connect_secured(&url, member.as_ref()).await else {
        panic!("an untrusted host must not get a channel");
    };
    assert!(err.to_string().contains("peer refused"), "got: {err}");

    assert_eq!(relay.plaintext_rpc_frames(), 0);
}

/// A revocation that reaches the host mid-session ends it at the next
/// sealed frame: the client's call fails and the link is down.
#[tokio::test]
async fn device_channel_ends_when_membership_is_revoked() {
    let relay = FakeRelay::start().await;
    let host_authority = MemberAuthority::new(0x11, true);
    let client_authority = MemberAuthority::new(0x22, true);
    host_authority.trust(&client_authority);
    client_authority.trust(&host_authority);
    let _host = secured_host(
        &relay.edge_url(),
        host_authority.clone(),
        TestService::new("host-a"),
    );
    relay.wait_host_connected().await;

    let links = secured_cache(&relay.edge_url(), client_authority.clone());
    let client = links.client("dev-a").await.expect("secured dial");
    client
        .call("Echo", serde_json::json!({}))
        .await
        .expect("live before revocation");

    host_authority.revoke(&client_authority);
    let err = client
        .call("Echo", serde_json::json!({}))
        .await
        .expect_err("revoked peer gets no answer");
    assert!(
        matches!(err, RpcError::Closed | RpcError::Transport(_)),
        "got: {err}"
    );
    // The cache noticed the link drop; a redial is refused at the handshake.
    let redial = loop {
        match links.client("dev-a").await {
            Err(err) if err.to_string().contains("backing off") => {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            other => break other,
        }
    };
    assert!(
        redial.is_err(),
        "revoked member must not re-establish the channel"
    );
}

/// A configured authority that is NOT yet enrolled keeps the plaintext
/// path: the vault's "no compatibility layer" rule is about enrolled
/// profiles, not about unenrolled ones losing remote control.
#[tokio::test]
async fn unenrolled_authority_keeps_plaintext_relay() {
    let relay = FakeRelay::start().await;
    let host_authority = MemberAuthority::new(0x11, false);
    let client_authority = MemberAuthority::new(0x22, false);
    let _host = secured_host(
        &relay.edge_url(),
        host_authority,
        TestService::new("host-a"),
    );
    relay.wait_host_connected().await;

    let links = secured_cache(&relay.edge_url(), client_authority);
    let client = links.client("dev-a").await.expect("plaintext dial");
    assert!(client.call("Echo", serde_json::json!({})).await.is_ok());
    assert!(relay.plaintext_rpc_frames() > 0);
}

/// Live-edge variant: run the same host+client path through a real DeviceRoom DO.
/// `ZERON_EDGE_WS=http://127.0.0.1:26640 cargo test -p zeron-rpc -- --ignored live_edge`
/// (dev-mode edge; ZERON_EDGE_TOKEN defaults to a fixed dev user id).
#[tokio::test]
#[ignore = "needs a running edge (set ZERON_EDGE_WS)"]
async fn live_edge_relay_round_trip() {
    let Ok(edge_url) = std::env::var("ZERON_EDGE_WS") else {
        panic!("set ZERON_EDGE_WS to the edge base URL (e.g. http://127.0.0.1:26640)");
    };
    let token = std::env::var("ZERON_EDGE_TOKEN").unwrap_or_else(|_| "relay-live-test".into());
    let device_id = format!("relay-live-{}", uuid::Uuid::new_v4());

    let service = TestService::new("live-host");
    let mut config = HostRelayConfig::new(
        edge_url.clone(),
        device_id.clone(),
        Arc::new(StaticToken(token.clone())),
    );
    config.retry = Duration::from_millis(500);
    let _host = HostRelay::spawn(config, service, noop_nudge());

    let mut cache_config = LinkCacheConfig::new(edge_url, Arc::new(StaticToken(token)));
    cache_config.probe_timeout = Duration::from_secs(5);
    let links = LinkCache::new(cache_config);

    // The host claims the room asynchronously; retry the dial until it answers.
    let client = loop {
        match links.client(&device_id).await {
            Ok(client) => break client,
            Err(err) => {
                eprintln!("dial retry: {err}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    let echoed = client
        .call("Echo", serde_json::json!({ "live": true }))
        .await
        .expect("echo");
    assert_eq!(echoed["host"], "live-host");
    assert_eq!(echoed["params"]["live"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn zombie_relay_path_trips_the_echo_deadline() {
    // 2026-08-19 incident shape: the client↔edge leg stays healthy (pongs
    // flow), the edge↔host leg is dead — the link used to sit "open" for
    // minutes, retrying frames into the void, until an unrelated host-session
    // cycle exposed it. The app-level echo must rule the link dead within its
    // deadline instead.
    zeron_rpc::device_room::set_client_liveness_for_tests(
        Duration::from_millis(100),
        Duration::from_millis(600),
    );
    let relay = FakeRelay::start().await;
    let service = TestService::new("host-a");
    let _host = HostRelay::spawn(relay_config(&relay.edge_url(), 100), service, noop_nudge());
    relay.wait_host_connected().await;

    let url = device_room_ws_url(&relay.edge_url(), "dev-a", "client", Some("c-echo"), "tok");
    let link = DeviceLink::connect(&url).await.expect("client link dials");
    // Healthy phase: echoes flow, the deadline never trips.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!link.is_closed(), "echoing link must stay open");

    relay.set_blackhole_host_bound(true);
    wait_until(|| link.is_closed()).await;
    assert_eq!(
        link.closed().borrow().as_deref(),
        Some("host echo silent"),
        "the zombie path must be ruled dead by the echo deadline"
    );
    // Restore the production clocks for the rest of the process's tests.
    zeron_rpc::device_room::set_client_liveness_for_tests(Duration::ZERO, Duration::ZERO);
}
