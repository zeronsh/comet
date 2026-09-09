//! Full authenticated Worker → catalog → SDP/ICE → P2P → HTTP integration.
//! Run against `wrangler dev --local --var AUTH_MODE:dev --port 27641`:
//! ZERON_PREVIEW_TEST_EDGE=http://127.0.0.1:27641 cargo test -p zeron-preview --test coordinator -- --ignored
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use zeron_preview::{
    catalog::Catalog,
    discovery::Listener,
    mux::{self, BoxIo, Connector},
    peer::Peers,
    proxy::{self, Router},
    signaling::{self, Config, TokenSource},
};
struct Token(String);
#[async_trait::async_trait]
impl TokenSource for Token {
    async fn token(&self) -> Option<String> {
        Some(self.0.clone())
    }
}
struct Backend(Catalog);
#[async_trait::async_trait]
impl Connector for Backend {
    async fn connect(&self, id: &str) -> anyhow::Result<BoxIo> {
        let route = self
            .0
            .local_route(id)
            .ok_or_else(|| anyhow::anyhow!("unknown service"))?;
        Ok(Box::new(TcpStream::connect(route.listener.address).await?))
    }
}
#[tokio::test]
#[ignore = "requires a local Worker in AUTH_MODE=dev"]
async fn authenticated_coordinator_pairs_devices_for_large_http_preview() {
    let edge = std::env::var("ZERON_PREVIEW_TEST_EDGE")
        .expect("set ZERON_PREVIEW_TEST_EDGE to the local dev Worker");
    tokio::time::timeout(Duration::from_secs(60), async {
        let temp = tempfile::tempdir().unwrap(); let stop = CancellationToken::new();
        let host = Catalog::open(temp.path().join("host.json"),"host".into(),"MacBook".into()).unwrap();
        let viewer = Catalog::open(temp.path().join("viewer.json"),"viewer".into(),"Desktop".into()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket,_) = listener.accept().await.unwrap(); let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") { headers.push(socket.read_u8().await.unwrap()); }
            let request = String::from_utf8(headers).unwrap(); assert!(request.to_lowercase().contains("host: macbook.project.localhost:"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4194304\r\nContent-Type: application/octet-stream\r\n\r\n").await.unwrap();
            for _ in 0..512 { socket.write_all(&[42;8192]).await.unwrap(); }
            socket.shutdown().await.unwrap();
        });
        host.replace_local(vec![("/work/project".into(),Listener { pid:std::process::id(),parent:1,cwd:"/work/project".into(),args:vec!["node".into(),"vite".into()],started_at:1,address,zeron_owned:true })]).unwrap();
        let host_backend = Arc::new(Backend(host.clone())); let viewer_backend = Arc::new(Backend(viewer.clone()));
        let (host_peers, host_output) = Peers::new("host".into(),host_backend,stop.child_token());
        let (viewer_peers, viewer_output) = Peers::new("viewer".into(),viewer_backend.clone(),stop.child_token());
        let tokens: Arc<dyn TokenSource> = Arc::new(Token(format!("{}@preview-test",uuid::Uuid::new_v4())));
        let config = || Config { edge_url:edge.clone(),org_id:"preview-test".into(),tokens:tokens.clone() };
        let host_signal = tokio::spawn(signaling::run(config(),host.clone(),host_peers,host_output,stop.child_token()));
        let viewer_signal = tokio::spawn(signaling::run(config(),viewer.clone(),viewer_peers.clone(),viewer_output,stop.child_token()));
        let (port, listeners) = proxy::serve(Router { catalog:viewer.clone(),local:mux::local(viewer_backend,stop.child_token()),peers:viewer_peers },0,stop.child_token()).await.unwrap(); viewer.set_proxy_status(port,None);
        let mut changes = viewer.subscribe();
        changes.wait_for(|snapshot|snapshot.services.len()==1).await.unwrap();
        let service = viewer.snapshot().services[0].clone(); assert_eq!(service.device_id,"host"); assert_eq!(service.hostname,"macbook.project.localhost");
        let client = reqwest::Client::builder().no_proxy().resolve(&service.hostname,([127,0,0,1],port).into()).build().unwrap();
        let response = client.get(service.url(port)).send().await.unwrap(); assert_eq!(response.status(),200);
        let bytes = response.bytes().await.unwrap(); assert_eq!(bytes.len(),4194304); assert!(bytes.iter().all(|b|*b==42));
        server.await.unwrap(); stop.cancel(); host_signal.await.unwrap(); viewer_signal.await.unwrap(); for listener in listeners { listener.await.unwrap(); }
    }).await.expect("coordinated P2P preview did not complete");
}
