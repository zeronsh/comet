//! Bounded HTTP bridge carried over an authenticated DeviceRoom preview frame.
//!
//! The service id is resolved in the hosting process's catalog; a browser never
//! supplies a host or port.
use crate::{catalog::Catalog, discovery};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request,
    header::{self, HeaderMap, HeaderName, HeaderValue},
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};

pub const KIND: &str = "preview";
pub const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct RequestHead {
    service: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct ResponseHead {
    status: u16,
    headers: Vec<(String, String)>,
}

fn pack<T: Serialize>(head: &T, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(body.len() <= MAX_BODY, "preview body exceeds limit");
    let head = serde_json::to_vec(head)?;
    anyhow::ensure!(head.len() <= 64 * 1024, "preview headers exceed limit");
    let mut result = Vec::with_capacity(4 + head.len() + body.len());
    result.extend_from_slice(&(head.len() as u32).to_be_bytes());
    result.extend_from_slice(&head);
    result.extend_from_slice(body);
    Ok(result)
}

fn unpack<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<(T, &[u8])> {
    anyhow::ensure!(bytes.len() >= 4, "truncated preview frame");
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    anyhow::ensure!(
        size <= 64 * 1024 && 4 + size <= bytes.len(),
        "invalid preview frame"
    );
    let head = serde_json::from_slice(&bytes[4..4 + size])?;
    let body = &bytes[4 + size..];
    anyhow::ensure!(body.len() <= MAX_BODY, "preview body exceeds limit");
    Ok((head, body))
}

pub fn encode_request(
    service: &str,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
    body: &[u8],
) -> anyhow::Result<Vec<u8>> {
    pack(
        &RequestHead {
            service: service.into(),
            method: method.into(),
            path: path.into(),
            headers,
        },
        body,
    )
}

pub fn decode_response(bytes: &[u8]) -> anyhow::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let (head, body): (ResponseHead, _) = unpack(bytes)?;
    Ok((head.status, head.headers, body.to_vec()))
}

pub async fn serve(catalog: &Catalog, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (head, body): (RequestHead, _) = unpack(bytes)?;
    anyhow::ensure!(
        !head.service.is_empty()
            && head.service.len() <= 128
            && !head.path.is_empty()
            && head.path.starts_with('/')
            && !head.path.starts_with("//")
            && !head.path.contains('\0')
            && !head.path.contains("http://")
            && !head.path.contains("https://"),
        "invalid preview request"
    );
    let route = catalog
        .local_route(&head.service)
        .ok_or_else(|| anyhow::anyhow!("preview service stopped"))?;
    let expected = route.listener.clone();
    let valid = tokio::task::spawn_blocking(move || {
        discovery::listeners().iter().any(|actual| {
            actual.pid == expected.pid
                && actual.started_at == expected.started_at
                && actual.cwd == expected.cwd
                && actual.address == expected.address
        })
    })
    .await?;
    anyhow::ensure!(valid, "preview process changed; waiting for rediscovery");

    let method = hyper::Method::from_bytes(head.method.as_bytes())?;
    let uri = hyper::Uri::try_from(head.path.as_str())?;
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Full::new(Bytes::copy_from_slice(body)))?;
    copy_request_headers(
        request.headers_mut(),
        &head.headers,
        &route.service.hostname,
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let stream = tokio::net::TcpStream::connect(route.listener.address).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        // `connection` must be polled concurrently with the request, but it must
        // not outlive a cancelled/slow relay request.
        let connection = tokio::spawn(async move {
            let _ = connection.await;
        });
        let _connection_abort = AbortOnDrop(connection.abort_handle());
        let response = sender.send_request(request).await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                (!hop_by_hop(name.as_str()))
                    .then(|| {
                        value
                            .to_str()
                            .ok()
                            .map(|value| (name.to_string(), value.to_string()))
                    })
                    .flatten()
            })
            .collect();
        let body = collect_body(response.into_body()).await?;
        pack(&ResponseHead { status, headers }, &body)
    })
    .await
    .map_err(|_| anyhow::anyhow!("preview request timed out"))?
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn collect_body(mut body: hyper::body::Incoming) -> anyhow::Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(chunk) = frame.into_data() {
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_BODY,
                "preview body exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok(Bytes::from(bytes))
}

fn copy_request_headers(
    target: &mut HeaderMap,
    source: &[(String, String)],
    hostname: &str,
) -> anyhow::Result<()> {
    for (name, value) in source {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if hop_by_hop(name.as_str())
            || matches!(
                name.as_str(),
                "cookie"
                    | "authorization"
                    | "proxy-authorization"
                    | "host"
                    | "origin"
                    | "referer"
                    | "x-forwarded-for"
                    | "x-forwarded-host"
                    | "x-forwarded-proto"
            )
        {
            continue;
        }
        if let Ok(value) = HeaderValue::from_str(value) {
            target.append(name, value);
        }
    }
    target.insert(header::HOST, HeaderValue::from_str(hostname)?);
    Ok(())
}

fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
