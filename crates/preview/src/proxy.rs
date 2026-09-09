//! HTTP/1 reverse proxy. Hyper owns HTTP framing; upgrades carry their original
//! bytes through the same multiplexed stream, including WebSocket close frames.
use crate::{
    catalog::Catalog,
    mux::{Mux, Stream},
    peer::Peers,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{self, HeaderMap, HeaderValue},
};
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, sync::Arc};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

type Body = UnsyncBoxBody<Bytes, hyper::Error>;
fn tunnel_slot() -> anyhow::Result<tokio::sync::SemaphorePermit<'static>> {
    static TUNNELS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(128);
    Ok(TUNNELS.try_acquire()?)
}
#[derive(Clone)]
pub struct Router {
    pub catalog: Catalog,
    pub local: Mux,
    pub peers: Peers,
}
impl Router {
    async fn open(&self, device: &str, service: &str, websocket: bool) -> anyhow::Result<Stream> {
        if device == self.catalog.device_id() {
            self.local.open(service, websocket).await
        } else {
            self.peers.open(device, service, websocket).await
        }
    }
    /// WebKit on older macOS releases delegates `.localhost` DNS to the OS.
    /// Its per-domain CONNECT proxy reaches this same loopback HTTP listener
    /// without a hosts-file entry. Targets are catalog names at our port only.
    async fn connect_loopback(
        &self,
        mut request: Request<Incoming>,
    ) -> anyhow::Result<Response<Body>> {
        let authority = request
            .uri()
            .authority()
            .ok_or_else(|| anyhow::anyhow!("invalid preview CONNECT"))?;
        let port = self.catalog.snapshot().proxy_port;
        anyhow::ensure!(
            authority.port_u16() == Some(port)
                && self
                    .catalog
                    .by_hostname(&authority.host().to_ascii_lowercase())
                    .is_some(),
            "unknown preview CONNECT target"
        );
        let permit = tunnel_slot()?;
        let lifecycle = self.local.clone();
        let mut socket =
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! { _ = lifecycle.closed() => {}, _ = async {
                if let Ok(browser) = upgrade.await {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(browser), &mut socket).await;
                }
            } => {} }
        });
        Ok(Response::new(
            Full::new(Bytes::new())
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        ))
    }
    async fn request(&self, mut request: Request<Incoming>) -> anyhow::Result<Response<Body>> {
        if request.method() == hyper::Method::CONNECT {
            return self.connect_loopback(request).await;
        }
        anyhow::ensure!(
            request.uri().scheme().is_none(),
            "only origin-form HTTP requests are supported"
        );
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("missing preview hostname"))?
            .to_ascii_lowercase();
        let authority: hyper::http::uri::Authority = host.parse()?;
        anyhow::ensure!(
            authority.port_u16().unwrap_or(80) == self.catalog.snapshot().proxy_port,
            "incorrect preview proxy port"
        );
        let service = self.catalog.by_hostname(authority.host()).ok_or_else(|| {
            anyhow::anyhow!(
                "This preview is not running. Start the project's dev server and try again."
            )
        })?;
        let upgrade = request
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
            && connection_has(request.headers(), "upgrade");
        let tunnel_permit = upgrade.then(tunnel_slot).transpose()?;
        let browser_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
        let upstream_host = format!("localhost:{}", service.port);
        let preview_origin = format!("http://{host}");
        // Preserve the browser's origin and authority together. In particular,
        // Next.js Server Actions compare Origin with X-Forwarded-Host; rewriting
        // only Origin to the ephemeral backend would reject valid submissions.
        strip_hop_headers(request.headers_mut(), upgrade);
        request
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_str(&host)?);
        request
            .headers_mut()
            .insert("x-forwarded-host", HeaderValue::from_str(&host)?);
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("http"));
        // Never pass client-supplied proxy credentials to a project server.
        request.headers_mut().remove(header::PROXY_AUTHORIZATION);
        let stream = self.open(&service.device_id, &service.id, upgrade).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let connection = tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        // Aborting a disconnected browser's pending request tears down its
        // upstream mux stream too, even when no response headers arrive.
        let guard = AbortOnDrop(Some(connection.abort_handle()));
        let mut response = sender.send_request(request).await?;
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            anyhow::ensure!(upgrade, "unsolicited server upgrade");
            let server_upgrade = hyper::upgrade::on(&mut response);
            let browser_upgrade = browser_upgrade.unwrap();
            let lifecycle = self.local.clone();
            tokio::spawn(async move {
                let _guard = guard;
                let _permit = tunnel_permit;
                tokio::select! { _ = lifecycle.closed() => {}, _ = async {
                    if let Ok((browser, server)) = tokio::try_join!(browser_upgrade, server_upgrade) {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(browser), &mut TokioIo::new(server)).await;
                    }
                } => {} }
            });
        } else {
            // The response body's guard aborts the connection on cancellation;
            // it is retained until the last streamed byte is consumed.
            let (mut parts, body) = response.into_parts();
            strip_hop_headers(&mut parts.headers, false);
            rewrite_location(&mut parts.headers, &upstream_host, &preview_origin);
            return Ok(Response::from_parts(
                parts,
                GuardedBody {
                    body,
                    _guard: guard,
                }
                .boxed_unsync(),
            ));
        }
        strip_hop_headers(response.headers_mut(), true);
        Ok(response.map(BodyExt::boxed_unsync))
    }
}
struct AbortOnDrop(Option<tokio::task::AbortHandle>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
struct GuardedBody {
    body: Incoming,
    _guard: AbortOnDrop,
}
impl hyper::body::Body for GuardedBody {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}
fn connection_has(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(token))
}
fn strip_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    let nominated: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    for name in nominated {
        if !(upgrade && name == "upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(header::UPGRADE);
    }
}
fn rewrite_location(headers: &mut HeaderMap, upstream: &str, preview: &str) {
    if let Some(value) = headers.get(header::LOCATION).and_then(|v| v.to_str().ok()) {
        let prefix = format!("http://{upstream}");
        if let Some(path) = value.strip_prefix(&prefix).filter(|path| {
            path.is_empty()
                || path.starts_with('/')
                || path.starts_with('?')
                || path.starts_with('#')
        }) {
            if let Ok(value) = HeaderValue::from_str(&format!("{preview}{path}")) {
                headers.insert(header::LOCATION, value);
            }
        }
    }
}
fn error(message: String) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(message))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
/// Bind both loopback families: `.localhost` commonly resolves to ::1 first.
/// Fail explicitly if another application owns the stable port.
pub async fn serve(
    router: Router,
    port: u16,
    stop: CancellationToken,
) -> anyhow::Result<(u16, Vec<tokio::task::JoinHandle<()>>)> {
    let v4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let port = v4.local_addr()?.port();
    let mut sockets = vec![v4];
    match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
        Ok(v6) => sockets.push(v6),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT | libc::EADDRNOTAVAIL)
            ) => {}
        Err(error) => return Err(error.into()),
    }
    let slots = Arc::new(tokio::sync::Semaphore::new(128));
    let mut listeners = Vec::new();
    for listener in sockets {
        let router = router.clone();
        let stop = stop.clone();
        let slots = slots.clone();
        listeners.push(tokio::spawn(async move {
            loop {
                let accepted = tokio::select! { _ = stop.cancelled() => break, result = listener.accept() => result };
                let Ok((socket, _)) = accepted else { break; };
                let Ok(permit) = slots.clone().try_acquire_owned() else { drop(socket); continue; };
                let router = router.clone(); let stop = stop.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let service = hyper::service::service_fn(move |request| {
                        let router = router.clone();
                        async move { Ok::<_, Infallible>(router.request(request).await.unwrap_or_else(|e| error(e.to_string()))) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(std::time::Duration::from_secs(15));
                    tokio::select! { _ = stop.cancelled() => {}, _ = builder.serve_connection(TokioIo::new(socket), service).with_upgrades() => {} }
                });
            }
        }));
    }
    Ok((port, listeners))
}
