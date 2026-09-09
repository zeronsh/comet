//! Two-device preview diagnostic. Exchange `SIGNAL` lines over an authenticated
//! channel (for example Zeron terminal RPC); never publish SDP in release logs.
//! `peer-probe host <localhost-port>` serves that explicit backend; `peer-probe
//! client` requests it. Each process reads the other process's signals on stdin.
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeron_preview::{
    mux::{BoxIo, Connector},
    peer::{Peers, Signal},
};

struct Backend(Option<u16>);
#[async_trait::async_trait]
impl Connector for Backend {
    async fn connect(&self, service: &str) -> anyhow::Result<BoxIo> {
        anyhow::ensure!(service == "diagnostic", "unknown diagnostic service");
        let port = self
            .0
            .ok_or_else(|| anyhow::anyhow!("client has no backend"))?;
        Ok(Box::new(
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?,
        ))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("zeron_preview=debug,webrtc=warn")
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let host = args.first().is_some_and(|s| s == "host");
    anyhow::ensure!(
        host || args == ["client"],
        "usage: peer-probe host <port> | client"
    );
    let port = if host {
        Some(
            args.get(1)
                .ok_or_else(|| anyhow::anyhow!("missing backend port"))?
                .parse()?,
        )
    } else {
        None
    };
    let stop = CancellationToken::new();
    let (peers, mut outgoing) = Peers::new(
        if host { "a" } else { "b" }.into(),
        Arc::new(Backend(port)),
        stop.clone(),
    );
    let signals = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            println!("SIGNAL {}", serde_json::to_string(&message.signal).unwrap());
        }
    });
    let incoming_peers = peers.clone();
    let incoming = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = lines.next_line().await? {
            let signal: Signal = serde_json::from_str(&line)?;
            eprintln!("INPUT signal {}", signal.kind);
            incoming_peers
                .signal(if host { "b" } else { "a" }, signal)
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let result = if host {
        incoming.await??;
        Ok(())
    } else {
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(40), async {
            let mut stream = peers.open("a", "diagnostic", false).await?;
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut bytes = Vec::new();
            stream.take(8 * 1024 * 1024).read_to_end(&mut bytes).await?;
            anyhow::ensure!(
                bytes.starts_with(b"HTTP/1.0 200") || bytes.starts_with(b"HTTP/1.1 200"),
                "backend did not return HTTP 200"
            );
            println!(
                "PASS remote HTTP 200: {} bytes in {:.2}s",
                bytes.len(),
                start.elapsed().as_secs_f64()
            );
            Ok::<_, anyhow::Error>(())
        })
        .await?;
        incoming.abort();
        result
    };
    stop.cancel();
    peers.clear().await;
    signals.abort();
    result
}
