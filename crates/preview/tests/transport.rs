use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeron_preview::mux::{self, BoxIo, Connector};
struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Connector for Echo {
    async fn connect(&self, service: &str) -> anyhow::Result<BoxIo> {
        anyhow::ensure!(service == "echo", "unknown service");
        let (client, mut server) = tokio::io::duplex(16384);
        let active = self.0.clone();
        active.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let mut bytes = [0; 8192];
            loop {
                let Ok(length) = server.read(&mut bytes).await else {
                    break;
                };
                if length == 0 {
                    break;
                }
                if server.write_all(&bytes[..length]).await.is_err() {
                    break;
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(Box::new(client))
    }
}
#[tokio::test]
async fn independent_streams_large_bodies_half_close_and_cancellation() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let active = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let mux = mux::local(Arc::new(Echo(active.clone())), stop.clone());
        assert!(mux.open("unknown", false).await.is_err());
        let blocked = mux.open("echo", false).await.unwrap();
        let (blocked_read, mut blocked_write) = tokio::io::split(blocked);
        let blocked_sender = tokio::spawn(async move {
            let _ = blocked_write.write_all(&vec![7; 2 * 1024 * 1024]).await;
        });
        let mut tasks = Vec::new();
        for number in 0..8 {
            let mux = mux.clone();
            tasks.push(tokio::spawn(async move {
                let stream = mux.open("echo", number % 2 == 0).await.unwrap();
                let (mut read, mut write) = tokio::io::split(stream);
                let bytes = vec![number; 512 * 1024 + 17];
                let expected = bytes.clone();
                let send = async move {
                    write.write_all(&bytes).await.unwrap();
                    write.shutdown().await.unwrap();
                };
                let receive = async move {
                    let mut output = Vec::new();
                    read.read_to_end(&mut output).await.unwrap();
                    assert_eq!(output.len(), expected.len());
                    assert!(output == expected);
                };
                tokio::join!(send, receive);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        blocked_sender.abort();
        let _ = blocked_sender.await;
        drop(blocked_read);
        while active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.cancel();
    })
    .await
    .expect("mux stalled");
}
