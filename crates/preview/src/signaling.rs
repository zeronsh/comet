//! The coordinator receives presence, catalogs and SDP only. Reconnects obtain
//! fresh credentials and clear the previous connection's remote routes/peers.
use crate::{
    catalog::Catalog,
    peer::{OutgoingSignal, Peers, Signal},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use zeron_proto::PreviewService;
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Option<String>;
}
pub struct Config {
    pub edge_url: String,
    pub org_id: String,
    pub tokens: Arc<dyn TokenSource>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Incoming {
    Catalog {
        device: String,
        services: Vec<PreviewService>,
    },
    Gone {
        device: String,
    },
    Signal {
        from: String,
        signal: Signal,
    },
}
pub async fn run(
    config: Config,
    catalog: Catalog,
    peers: Peers,
    mut outgoing: mpsc::Receiver<OutgoingSignal>,
    stop: CancellationToken,
) {
    loop {
        let connected = connect(&config, &catalog, &peers, &mut outgoing);
        tokio::select! { _ = stop.cancelled() => break, result = connected => {
            if let Err(error) = result { tracing::debug!(%error, "preview coordinator disconnected"); }
        } }
        catalog.clear_remote();
        peers.clear().await;
        while outgoing.try_recv().is_ok() {} // signaling from the old lease is stale
        tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
    }
    catalog.clear_remote();
    peers.clear().await;
}
async fn connect(
    config: &Config,
    catalog: &Catalog,
    peers: &Peers,
    outgoing: &mut mpsc::Receiver<OutgoingSignal>,
) -> anyhow::Result<()> {
    let token = config
        .tokens
        .token()
        .await
        .ok_or_else(|| anyhow::anyhow!("preview authentication unavailable"))?;
    let mut url = reqwest::Url::parse(&config.edge_url)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid preview coordinator URL"))?;
    url.set_path(&format!("/preview/{}/ws", config.org_id));
    url.query_pairs_mut()
        .clear()
        .append_pair("device", catalog.device_id());
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse()?);
    let mut websocket_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    websocket_config.max_message_size = Some(1024 * 1024);
    websocket_config.max_frame_size = Some(1024 * 1024);
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config), false),
    )
    .await??;
    let (mut write, mut read) = socket.split();
    let mut changes = catalog.subscribe();
    let mut advertised = catalog.local_services();
    write
        .send(Message::Text(
            serde_json::json!({"type":"catalog", "services":advertised}).to_string(),
        ))
        .await?;
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    let mut last_message = tokio::time::Instant::now();
    // SDP gathering must not block the socket heartbeat or other devices.
    let mut negotiations = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                anyhow::ensure!(last_message.elapsed() < Duration::from_secs(40), "preview coordinator lease expired");
                write.send(Message::Text("ping".into())).await?;
            }
            _ = changes.changed() => {
                let next = catalog.local_services();
                if next != advertised { write.send(Message::Text(serde_json::json!({"type":"catalog", "services":next}).to_string())).await?; advertised = next; }
            }
            Some(message) = outgoing.recv() => {
                write.send(Message::Text(serde_json::json!({"type":"signal", "to":message.to, "signal":message.signal}).to_string())).await?;
            }
            Some(_) = negotiations.join_next(), if !negotiations.is_empty() => {}
            message = read.next() => {
                let message = message.ok_or_else(|| anyhow::anyhow!("preview coordinator closed"))??;
                last_message = tokio::time::Instant::now();
                match message {
                    Message::Text(text) if text == "pong" => {},
                    Message::Text(text) => {
                        anyhow::ensure!(text.len() <= 1024*1024, "oversized preview coordinator message");
                        match serde_json::from_str::<Incoming>(&text)? {
                            Incoming::Catalog { device, services } => catalog.set_remote(&device, services)?,
                            Incoming::Gone { device } => { catalog.remove_remote(&device); peers.remove(&device).await; }
                            Incoming::Signal { from, signal } => {
                                anyhow::ensure!(negotiations.len() < 16, "too many concurrent preview negotiations");
                                let peers = peers.clone();
                                negotiations.spawn(async move { if let Err(error) = peers.signal(&from, signal).await { tracing::debug!(%error, "preview negotiation failed"); } });
                            }
                        }
                    }
                    Message::Ping(bytes) => write.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => anyhow::bail!("preview coordinator closed"),
                    Message::Binary(_) => anyhow::bail!("application traffic is not allowed on preview signaling"),
                    _ => {}
                }
            }
        }
    }
}
