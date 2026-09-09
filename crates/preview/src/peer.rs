//! Authenticated signaling supplies SDP/DTLS fingerprints. Preview bytes only
//! use the resulting reliable, ordered DataChannel; there is no edge byte relay.
use crate::mux::{Connector, Mux, Stream, Transport};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
        RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Signal {
    pub kind: String,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdp: Option<RTCSessionDescription>,
}
#[derive(Clone)]
pub struct OutgoingSignal {
    pub to: String,
    pub signal: Signal,
}
struct Peer {
    session: String,
    pc: Arc<dyn PeerConnection>,
    ready: watch::Receiver<Option<Mux>>,
    gathered: watch::Receiver<bool>,
    stop: CancellationToken,
}
impl Peer {
    async fn close(&self) {
        self.stop.cancel();
        let _ = self.pc.close().await;
    }
}
struct Inner {
    device: String,
    connector: Arc<dyn Connector>,
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    requested: Mutex<HashMap<String, tokio::time::Instant>>,
    output: mpsc::Sender<OutgoingSignal>,
    changed: Arc<Notify>,
    stop: CancellationToken,
    ice_servers: Vec<RTCIceServer>,
}
#[derive(Clone)]
pub struct Peers(Arc<Inner>);
impl Peers {
    pub fn new(
        device: String,
        connector: Arc<dyn Connector>,
        stop: CancellationToken,
    ) -> (Self, mpsc::Receiver<OutgoingSignal>) {
        let (output, receiver) = mpsc::channel(64);
        (
            Self(Arc::new(Inner {
                device,
                connector,
                peers: Mutex::new(HashMap::new()),
                requested: Mutex::new(HashMap::new()),
                output,
                changed: Arc::new(Notify::new()),
                stop,
                ice_servers: vec![RTCIceServer {
                    urls: vec![
                        "stun:stun.l.google.com:19302".into(),
                        "stun:stun1.l.google.com:19302".into(),
                    ],
                    ..Default::default()
                }],
            })),
            receiver,
        )
    }
    async fn send(&self, to: &str, signal: Signal) -> anyhow::Result<()> {
        tokio::select! { _ = self.0.stop.cancelled() => anyhow::bail!("preview networking stopped"), result = self.0.output.send(OutgoingSignal { to: to.into(), signal }) => { result?; Ok(()) } }
    }
    async fn create(&self, session: String, initiator: bool) -> anyhow::Result<Arc<Peer>> {
        let (ready, receiver) = watch::channel(None);
        let (gathered, gathering) = watch::channel(false);
        let stop = self.0.stop.child_token();
        let handler = Arc::new(Handler {
            ready,
            gathered,
            channel_claimed: AtomicBool::new(false),
            connector: self.0.connector.clone(),
            initiator,
            stop: stop.clone(),
            changed: self.0.changed.clone(),
        });
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(
                    RTCConfigurationBuilder::new()
                        .with_ice_servers(self.0.ice_servers.clone())
                        .build(),
                )
                .with_handler(handler.clone())
                .with_udp_addrs(vec!["0.0.0.0:0"])
                .with_data_channel_send_buffer_limit(256 * 1024)
                .with_sctp_receive_buffer_size(512 * 1024)
                .build()
                .await?,
        );
        if initiator {
            let channel = pc.create_data_channel("zeron-preview-v1", None).await?;
            handler.attach(channel);
        }
        Ok(Arc::new(Peer {
            session,
            pc,
            ready: receiver,
            gathered: gathering,
            stop,
        }))
    }
    async fn description(&self, peer: &Peer, offer: bool) -> anyhow::Result<RTCSessionDescription> {
        let description = if offer {
            peer.pc.create_offer(None).await?
        } else {
            peer.pc.create_answer(None).await?
        };
        peer.pc.set_local_description(description).await?;
        // Include the gathered ICE candidates in SDP. This avoids candidate /
        // remote-description races and keeps reconnect messages self-contained.
        let mut gathered = peer.gathered.clone();
        tokio::time::timeout(Duration::from_secs(12), gathered.wait_for(|done| *done)).await??;
        peer.pc
            .local_description()
            .await
            .context("missing local preview SDP")
    }
    async fn offer(&self, device: &str, session: String) -> anyhow::Result<()> {
        let mut peers = self.0.peers.lock().await;
        if peers.get(device).is_some_and(|p| !p.stop.is_cancelled()) {
            return Ok(());
        }
        anyhow::ensure!(
            peers.len() < 16 || peers.contains_key(device),
            "too many preview peers"
        );
        let peer = self.create(session, true).await?;
        if let Some(old) = peers.insert(device.into(), peer.clone()) {
            old.close().await;
        }
        drop(peers);
        let result = async {
            let sdp = self.description(&peer, true).await?;
            self.send(
                device,
                Signal {
                    kind: "offer".into(),
                    session: peer.session.clone(),
                    sdp: Some(sdp),
                },
            )
            .await
        }
        .await;
        if result.is_err() {
            peer.close().await;
        }
        result
    }
    /// Called only for messages stamped by the authenticated coordinator.
    pub async fn signal(&self, device: &str, signal: Signal) -> anyhow::Result<()> {
        anyhow::ensure!(
            device != self.0.device && !signal.session.is_empty() && signal.session.len() <= 128,
            "invalid preview pairing"
        );
        match signal.kind.as_str() {
            "connect" => {
                anyhow::ensure!(
                    self.0.device.as_str() < device && signal.sdp.is_none(),
                    "invalid preview initiator"
                );
                self.offer(device, signal.session).await?;
            }
            "offer" => {
                anyhow::ensure!(
                    device < self.0.device.as_str(),
                    "invalid preview offer sender"
                );
                let mut peers = self.0.peers.lock().await;
                anyhow::ensure!(
                    peers.len() < 16 || peers.contains_key(device),
                    "too many preview peers"
                );
                let peer = self.create(signal.session, false).await?;
                if let Some(old) = peers.insert(device.into(), peer.clone()) {
                    old.close().await;
                }
                drop(peers);
                let result = async {
                    peer.pc
                        .set_remote_description(signal.sdp.context("missing offer")?)
                        .await?;
                    let sdp = self.description(&peer, false).await?;
                    self.send(
                        device,
                        Signal {
                            kind: "answer".into(),
                            session: peer.session.clone(),
                            sdp: Some(sdp),
                        },
                    )
                    .await
                }
                .await;
                if result.is_err() {
                    peer.close().await;
                }
                result?;
            }
            "answer" => {
                anyhow::ensure!(
                    self.0.device.as_str() < device,
                    "invalid preview answer sender"
                );
                let peer = self
                    .0
                    .peers
                    .lock()
                    .await
                    .get(device)
                    .cloned()
                    .context("unexpected preview answer")?;
                anyhow::ensure!(peer.session == signal.session, "stale preview answer");
                peer.pc
                    .set_remote_description(signal.sdp.context("missing answer")?)
                    .await?;
            }
            _ => anyhow::bail!("unknown preview signaling message"),
        }
        self.0.changed.notify_waiters();
        Ok(())
    }
    pub async fn open(
        &self,
        device: &str,
        service: &str,
        websocket: bool,
    ) -> anyhow::Result<Stream> {
        let device = device.to_owned();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let existing = self.0.peers.lock().await.get(&device).cloned();
            if existing.as_ref().is_none_or(|p| p.stop.is_cancelled() || p.ready.borrow().as_ref().is_some_and(Mux::is_closed)) {
                if let Some(peer) = existing { peer.close().await; }
                let session = uuid::Uuid::new_v4().to_string();
                if self.0.device < device { self.offer(&device, session).await?; }
                else {
                    let should_request = {
                        let mut requested = self.0.requested.lock().await;
                        let now = tokio::time::Instant::now();
                        if requested.get(&device).is_some_and(|at| now.duration_since(*at) < Duration::from_secs(10)) { false }
                        else { requested.insert(device.clone(), now); true }
                    };
                    if should_request { self.send(&device, Signal { kind: "connect".into(), session, sdp: None }).await?; }
                }
            }
            loop {
                let changed = self.0.changed.notified();
                let mux = self.0.peers.lock().await.get(&device).and_then(|peer| peer.ready.borrow().clone());
                if let Some(mux) = mux.filter(|m| !m.is_closed()) { return mux.open(service, websocket).await; }
                tokio::select! { _ = self.0.stop.cancelled() => anyhow::bail!("preview networking stopped"), _ = changed => {}, _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
            }
        }).await;
        if result.is_err() {
            self.remove(&device).await;
        }
        result.context("Could not connect directly to this device. Check that both devices are online and their networks allow WebRTC.")?
    }
    pub async fn remove(&self, device: &str) {
        self.0.requested.lock().await.remove(device);
        if let Some(peer) = self.0.peers.lock().await.remove(device) {
            peer.close().await;
        }
        self.0.changed.notify_waiters();
    }
    pub async fn clear(&self) {
        self.0.requested.lock().await.clear();
        let peers = std::mem::take(&mut *self.0.peers.lock().await);
        for (_, peer) in peers {
            peer.close().await;
        }
        self.0.changed.notify_waiters();
    }
}
struct Handler {
    channel_claimed: AtomicBool,
    ready: watch::Sender<Option<Mux>>,
    gathered: watch::Sender<bool>,
    connector: Arc<dyn Connector>,
    initiator: bool,
    stop: CancellationToken,
    changed: Arc<Notify>,
}
impl Handler {
    fn attach(&self, channel: Arc<dyn DataChannel>) {
        if self.channel_claimed.swap(true, Ordering::SeqCst) {
            tokio::spawn(async move {
                let _ = channel.close().await;
            });
            return;
        }
        let ready = self.ready.clone();
        let connector = self.connector.clone();
        let stop = self.stop.clone();
        let initiator = self.initiator;
        let changed = self.changed.clone();
        tokio::spawn(async move {
            let valid = channel.label().await.is_ok_and(|s| s == "zeron-preview-v1")
                && channel.ordered().await.unwrap_or(false)
                && channel.max_retransmits().await.is_ok_and(|v| v.is_none())
                && channel
                    .max_packet_life_time()
                    .await
                    .is_ok_and(|v| v.is_none());
            if !valid {
                let _ = channel.close().await;
                return;
            }
            loop {
                let event = tokio::select! { _ = stop.cancelled() => return, event = channel.poll() => event };
                match event {
                    Some(DataChannelEvent::OnOpen) => {
                        if ready.borrow().is_some() {
                            let _ = channel.close().await;
                            return;
                        }
                        let mux = Mux::start(
                            Arc::new(ChannelTransport(channel)),
                            connector,
                            initiator,
                            stop.child_token(),
                        );
                        ready.send_replace(Some(mux));
                        changed.notify_waiters();
                        return;
                    }
                    None | Some(DataChannelEvent::OnClose) => return,
                    _ => {}
                }
            }
        });
    }
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.send_replace(true);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected
        ) {
            self.stop.cancel();
            self.ready.send_replace(None);
            self.changed.notify_waiters();
        }
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        self.attach(channel);
    }
}
struct ChannelTransport(Arc<dyn DataChannel>);
#[async_trait::async_trait]
impl Transport for ChannelTransport {
    async fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.0.send(bytes::BytesMut::from(bytes)).await?;
        Ok(())
    }
    async fn receive(&self) -> anyhow::Result<Vec<u8>> {
        loop {
            match self.0.poll().await {
                Some(DataChannelEvent::OnMessage(message)) => {
                    anyhow::ensure!(
                        message.data.len() <= 8197,
                        "oversized preview DataChannel frame"
                    );
                    return Ok(message.data.to_vec());
                }
                None | Some(DataChannelEvent::OnClose) => {
                    anyhow::bail!("preview DataChannel closed")
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Echo;
    #[async_trait::async_trait]
    impl Connector for Echo {
        async fn connect(&self, id: &str) -> anyhow::Result<crate::mux::BoxIo> {
            anyhow::ensure!(id == "service", "unknown service");
            let (client, server) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(server);
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
            Ok(Box::new(client))
        }
    }
    #[tokio::test]
    async fn actual_webrtc_pair_streams_in_both_directions() {
        let stop = CancellationToken::new();
        let (mut a, mut a_out) = Peers::new("a".into(), Arc::new(Echo), stop.clone());
        let (mut b, mut b_out) = Peers::new("b".into(), Arc::new(Echo), stop.clone());
        Arc::get_mut(&mut a.0).unwrap().ice_servers.clear();
        Arc::get_mut(&mut b.0).unwrap().ice_servers.clear();
        let peer_b = b.clone();
        let forward_a = tokio::spawn(async move {
            while let Some(message) = a_out.recv().await {
                peer_b.signal("a", message.signal).await.unwrap();
            }
        });
        let peer_a = a.clone();
        let forward_b = tokio::spawn(async move {
            while let Some(message) = b_out.recv().await {
                peer_a.signal("b", message.signal).await.unwrap();
            }
        });
        // A page's initial burst must pair once, rather than flood signaling
        // with one connection request per asset.
        futures::future::join_all((0..8).map(|_| async {
            let mut stream = b.open("a", "service", false).await.unwrap();
            stream.write_all(b"asset").await.unwrap();
            stream.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"asset");
        }))
        .await;
        for (from, to) in [(&b, "a"), (&a, "b")] {
            let stream = from.open(to, "service", true).await.unwrap();
            let (mut read, mut write) = tokio::io::split(stream);
            let expected = vec![42; 1024 * 1024 + 3];
            let bytes = expected.clone();
            tokio::time::timeout(Duration::from_secs(15), async {
                let sender = async {
                    write.write_all(&bytes).await.unwrap();
                    write.shutdown().await.unwrap();
                };
                let receiver = async {
                    let mut received = Vec::new();
                    read.read_to_end(&mut received).await.unwrap();
                    assert_eq!(received.len(), expected.len());
                    assert!(received == expected);
                };
                tokio::join!(sender, receiver);
            })
            .await
            .expect("P2P stream stalled");
        }
        stop.cancel();
        a.clear().await;
        b.clear().await;
        forward_a.abort();
        forward_b.abort();
    }
}
