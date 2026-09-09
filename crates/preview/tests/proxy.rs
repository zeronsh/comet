use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response,
    body::{Frame, Incoming},
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::Role},
};
use tokio_util::sync::CancellationToken;
use zeron_preview::{
    catalog::Catalog,
    discovery::Listener,
    mux::{self, BoxIo, Connector},
    peer::Peers,
    proxy::{self, Router},
};
type Body = UnsyncBoxBody<Bytes, hyper::Error>;
fn full(value: impl Into<Bytes>) -> Body {
    Full::new(value.into())
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}
struct Count(Arc<AtomicUsize>);
impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
async fn server(stop: CancellationToken, active: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let socket = tokio::select! { _ = stop.cancelled() => break, value = listener.accept() => value.unwrap().0 };
            active.fetch_add(1, Ordering::SeqCst);
            let guard = Count(active.clone());
            let stop = stop.clone();
            tokio::spawn(async move {
                let _guard = guard;
                let service = hyper::service::service_fn(
                    move |mut request: Request<Incoming>| async move {
                        let response = match request.uri().path() {
                            "/echo" => Response::new(request.into_body().boxed_unsync()),
                            "/stream" | "/infinite" => {
                                let max = if request.uri().path() == "/infinite" {
                                    usize::MAX
                                } else {
                                    20
                                };
                                let body = futures::stream::unfold(0, move |index| async move {
                                    if index >= max {
                                        return None;
                                    }
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                    Some((
                                        Ok::<_, hyper::Error>(Frame::data(Bytes::from(vec![
                                            42;
                                            8192
                                        ]))),
                                        index + 1,
                                    ))
                                });
                                Response::new(StreamBody::new(body).boxed_unsync())
                            }
                            "/headers" => {
                                let values = serde_json::json!({"host":request.headers()["host"].to_str().unwrap(), "forwarded":request.headers()["x-forwarded-host"].to_str().unwrap(), "hop":request.headers().contains_key("x-hop"), "origin":request.headers().get("origin").and_then(|v|v.to_str().ok()), "query":request.uri().query()});
                                let mut response = Response::new(full(values.to_string()));
                                response
                                    .headers_mut()
                                    .append("set-cookie", "a=1; Path=/".parse().unwrap());
                                response
                                    .headers_mut()
                                    .append("set-cookie", "b=2; Path=/".parse().unwrap());
                                response
                            }
                            "/redirect" => Response::builder()
                                .status(302)
                                .header(
                                    "location",
                                    format!("http://localhost:{port}/headers?q=yes"),
                                )
                                .body(full(""))
                                .unwrap(),
                            "/ws" => {
                                let accept =
                                    tokio_tungstenite::tungstenite::handshake::derive_accept_key(
                                        request.headers()["sec-websocket-key"].as_bytes(),
                                    );
                                let upgrade = hyper::upgrade::on(&mut request);
                                tokio::spawn(async move {
                                    let mut socket = WebSocketStream::from_raw_socket(
                                        TokioIo::new(upgrade.await.unwrap()),
                                        Role::Server,
                                        None,
                                    )
                                    .await;
                                    while let Some(Ok(message)) = socket.next().await {
                                        if message.is_close() {
                                            let _ = socket.flush().await;
                                            break;
                                        }
                                        if message.is_text() || message.is_binary() {
                                            if socket.send(message).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                });
                                Response::builder()
                                    .status(101)
                                    .header("connection", "upgrade")
                                    .header("upgrade", "websocket")
                                    .header("sec-websocket-accept", accept)
                                    .body(full(""))
                                    .unwrap()
                            }
                            _ => Response::new(full(format!("server:{port}"))),
                        };
                        Ok::<_, Infallible>(response)
                    },
                );
                tokio::select! { _ = stop.cancelled() => {}, _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket), service).with_upgrades() => {} }
            });
        }
    });
    port
}
struct Backend(Catalog);
#[async_trait::async_trait]
impl Connector for Backend {
    async fn connect(&self, id: &str) -> anyhow::Result<BoxIo> {
        Ok(Box::new(
            TcpStream::connect(self.0.local_route(id).unwrap().listener.address).await?,
        ))
    }
}
fn observe(catalog: &Catalog, port: u16, pid: u32) {
    catalog
        .replace_local(vec![(
            "/work/app".into(),
            Listener {
                pid,
                parent: 1,
                cwd: "/work/app".into(),
                args: vec!["node".into(), "vite".into()],
                started_at: pid as u64,
                address: ([127, 0, 0, 1], port).into(),
                zeron_owned: true,
            },
        )])
        .unwrap();
}
#[tokio::test]
async fn streaming_headers_websocket_cancellation_and_restart() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let temp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(
            temp.path().join("names.json"),
            "local".into(),
            "Laptop".into(),
        )
        .unwrap();
        let stop = CancellationToken::new();
        let backend_stop = stop.child_token();
        let active = Arc::new(AtomicUsize::new(0));
        let backend_port = server(backend_stop.clone(), active.clone()).await;
        observe(&catalog, backend_port, 1);
        let connector = Arc::new(Backend(catalog.clone()));
        let local = mux::local(connector.clone(), stop.clone());
        let (peers, _) = Peers::new("local".into(), connector, stop.clone());
        let (port, _listeners) = proxy::serve(
            Router {
                catalog: catalog.clone(),
                local,
                peers,
            },
            0,
            stop.clone(),
        )
        .await
        .unwrap();
        catalog.set_proxy_status(port, None);
        let service = catalog.snapshot().services[0].clone();
        let url = service.url(port);
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve(&service.hostname, ([127, 0, 0, 1], port).into())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut tunnel = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        tunnel
            .write_all(
                format!(
                    "CONNECT {}:{port} HTTP/1.1\r\nHost: {}:{port}\r\n\r\n",
                    service.hostname, service.hostname
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = Vec::new();
        while !reply.ends_with(b"\r\n\r\n") {
            reply.push(tunnel.read_u8().await.unwrap());
        }
        assert!(reply.starts_with(b"HTTP/1.1 200"));
        tunnel
            .write_all(
                format!(
                    "GET / HTTP/1.1\r\nHost: {}:{port}\r\nConnection: close\r\n\r\n",
                    service.hostname
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut page = String::new();
        tunnel.read_to_string(&mut page).await.unwrap();
        assert!(page.contains(&format!("server:{backend_port}")));
        drop(tunnel);
        let rejected = client
            .request(reqwest::Method::CONNECT, format!("http://127.0.0.1:{port}"))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), 502);
        drop(rejected);
        let response = client
            .get(format!("{url}/headers?q=yes"))
            .header("connection", "x-hop")
            .header("x-hop", "remove")
            .header("origin", &url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        let headers: serde_json::Value = response.json().await.unwrap();
        assert_eq!(headers["host"], format!("{}:{port}", service.hostname));
        assert_eq!(headers["forwarded"], format!("{}:{port}", service.hostname));
        assert_eq!(headers["hop"], false);
        assert_eq!(headers["query"], "q=yes");
        assert_eq!(headers["origin"], url);
        let body = vec![5u8; 2 * 1024 * 1024 + 17];
        let received = client
            .post(format!("{url}/echo"))
            .body(body.clone())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(received.as_ref() == body);
        let started = tokio::time::Instant::now();
        let mut streaming = client
            .get(format!("{url}/stream"))
            .send()
            .await
            .unwrap()
            .bytes_stream();
        let first = streaming.next().await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_millis(300));
        let mut length = first.len();
        while let Some(chunk) = streaming.next().await {
            length += chunk.unwrap().len();
        }
        assert_eq!(length, 20 * 8192);
        let response = client.get(format!("{url}/redirect")).send().await.unwrap();
        assert_eq!(
            response.headers()["location"],
            format!("{url}/headers?q=yes")
        );
        drop(response);
        let mut request = format!("ws://{}:{port}/ws", service.hostname)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert("origin", url.parse().unwrap());
        let tcp = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(request, tcp).await.unwrap();
        for msg in [
            Message::Text("hot-update".into()),
            Message::Binary(vec![7; 256 * 1024]),
        ] {
            ws.send(msg.clone()).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap() == msg);
        }
        ws.close(None).await.unwrap();
        let _ = ws.next().await;
        drop(ws);
        let mut infinite = client
            .get(format!("{url}/infinite"))
            .send()
            .await
            .unwrap()
            .bytes_stream();
        infinite.next().await.unwrap().unwrap();
        drop(infinite);
        while active.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        backend_stop.cancel();
        catalog.replace_local(Vec::new()).unwrap();
        assert_eq!(client.get(&url).send().await.unwrap().status(), 502);
        let next_port = server(stop.child_token(), active).await;
        observe(&catalog, next_port, 2);
        assert_eq!(catalog.snapshot().services[0].url(port), url);
        assert_eq!(
            client.get(&url).send().await.unwrap().text().await.unwrap(),
            format!("server:{next_port}")
        );
        assert_eq!(
            client
                .get(format!("http://127.0.0.1:{port}"))
                .send()
                .await
                .unwrap()
                .status(),
            502
        );
        stop.cancel();
    })
    .await
    .expect("proxy test stalled");
}
