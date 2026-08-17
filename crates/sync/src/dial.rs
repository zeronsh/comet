//! Happy-eyeballs WebSocket dialing (RFC 8305, simplified).
//!
//! `tokio_tungstenite::connect_async` tries resolved addresses SEQUENTIALLY
//! with no per-address bound, so a network that advertises IPv6 but blackholes
//! it (captive portals, airplane wifi) hangs the first SYN until the caller's
//! whole-dial timeout — and the retry repeats the identical order, so the
//! socket never comes up at all. Browsers (and this codebase's reqwest stack
//! via hyper-util) race address families and connect fine on the same network;
//! every WebSocket dial goes through here so they behave the same way:
//! resolve, interleave v6/v4, start one TCP attempt every [`STAGGER`] (or
//! immediately when the previous attempt fails), first connected stream wins
//! and the losers are dropped mid-flight.
//!
//! The winner also gets `TCP_NODELAY` — the protocols upstairs exchange
//! back-to-back small frames (join → eph-join, ack → push) that Nagle would
//! pointlessly pair with delayed ACKs.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{Error as WsError, UrlError};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Delay before starting the next address attempt while one is still pending
/// (RFC 8305 §5's "Connection Attempt Delay"; 250ms is its recommended value).
const STAGGER: Duration = Duration::from_millis(250);

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dial `url` (ws/wss) with happy-eyeballs TCP racing, then run TLS + the
/// WebSocket handshake on the winning stream. A success also broadcasts
/// [`crate::wake::notify_online`] so sibling sockets waiting out a reconnect
/// backoff redial immediately instead of sleeping through the recovery.
pub async fn connect_ws(url: &str) -> Result<WsStream, WsError> {
    let request = url.into_client_request()?;
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or(WsError::Url(UrlError::NoHostName))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") => 443,
        _ => 80,
    });
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .collect();
    let stream = race_connect(interleave_families(addrs))
        .await
        .map_err(|err| io::Error::new(err.kind(), format!("{host}:{port}: {err}")))?;
    // Best-effort: a socket that works without NODELAY beats no socket.
    let _ = stream.set_nodelay(true);
    let (ws, _response) =
        tokio_tungstenite::client_async_tls_with_config(request, stream, None, None).await?;
    crate::wake::notify_online();
    Ok(ws)
}

/// Race TCP connects over an already-ordered address list: one new attempt per
/// [`STAGGER`] tick (immediately on a failure), first success wins, losers are
/// dropped mid-flight.
async fn race_connect(mut queue: VecDeque<SocketAddr>) -> io::Result<TcpStream> {
    let Some(first) = queue.pop_front() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no addresses resolved",
        ));
    };
    let mut pending = FuturesUnordered::new();
    pending.push(TcpStream::connect(first));
    let stagger = tokio::time::sleep(STAGGER);
    tokio::pin!(stagger);

    loop {
        tokio::select! {
            res = pending.next(), if !pending.is_empty() => match res {
                Some(Ok(stream)) => return Ok(stream),
                Some(Err(err)) => {
                    // A failure advances the schedule immediately (§5).
                    match queue.pop_front() {
                        Some(addr) => {
                            pending.push(TcpStream::connect(addr));
                            stagger.as_mut().reset(tokio::time::Instant::now() + STAGGER);
                        }
                        // Last attempt standing: its error is the verdict.
                        None if pending.is_empty() => return Err(err),
                        None => {}
                    }
                }
                None => unreachable!("guarded on !pending.is_empty()"),
            },
            _ = stagger.as_mut(), if !queue.is_empty() => {
                if let Some(addr) = queue.pop_front() {
                    pending.push(TcpStream::connect(addr));
                }
                stagger.as_mut().reset(tokio::time::Instant::now() + STAGGER);
            }
        }
    }
}

/// Alternate address families starting with the resolver's first pick,
/// preserving resolver order within each family (RFC 8305 §4).
fn interleave_families(addrs: Vec<SocketAddr>) -> VecDeque<SocketAddr> {
    let first_is_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (preferred, other): (Vec<_>, Vec<_>) =
        addrs.into_iter().partition(|a| a.is_ipv6() == first_is_v6);
    let mut out = VecDeque::with_capacity(preferred.len() + other.len());
    let (mut preferred, mut other) = (preferred.into_iter(), other.into_iter());
    loop {
        match (preferred.next(), other.next()) {
            (None, None) => break,
            (a, b) => {
                out.extend(a);
                out.extend(b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interleaves_families_from_resolver_first_pick() {
        let ordered = interleave_families(vec![
            addr("[2001:db8::1]:443"),
            addr("[2001:db8::2]:443"),
            addr("192.0.2.1:443"),
            addr("192.0.2.2:443"),
            addr("192.0.2.3:443"),
        ]);
        let got: Vec<_> = ordered.into_iter().collect();
        assert_eq!(
            got,
            vec![
                addr("[2001:db8::1]:443"),
                addr("192.0.2.1:443"),
                addr("[2001:db8::2]:443"),
                addr("192.0.2.2:443"),
                addr("192.0.2.3:443"),
            ]
        );
    }

    #[test]
    fn single_family_passes_through() {
        let ordered = interleave_families(vec![addr("192.0.2.1:80"), addr("192.0.2.2:80")]);
        let got: Vec<_> = ordered.into_iter().collect();
        assert_eq!(got, vec![addr("192.0.2.1:80"), addr("192.0.2.2:80")]);
    }

    #[tokio::test]
    async fn blackholed_first_address_does_not_block_the_working_one() {
        // A listener that accepts (the "IPv4 works" side) behind an RFC 5737
        // TEST-NET address that never answers (the blackholed side). The race
        // must connect via the listener in ~STAGGER instead of hanging on the
        // black hole the way a sequential dial does.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let queue: VecDeque<SocketAddr> = vec![
            addr("192.0.2.1:9"),
            format!("127.0.0.1:{port}").parse().unwrap(),
        ]
        .into();
        let started = std::time::Instant::now();
        let stream = tokio::time::timeout(Duration::from_secs(5), race_connect(queue))
            .await
            .expect("raced connect hung on the blackholed address")
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "raced connect took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn all_failures_surface_the_last_error() {
        // Two closed ports on loopback: both refuse, the error must surface
        // rather than the loop hanging or panicking.
        let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (pa, pb) = (
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        );
        drop((a, b));
        let queue: VecDeque<SocketAddr> = vec![
            format!("127.0.0.1:{pa}").parse().unwrap(),
            format!("127.0.0.1:{pb}").parse().unwrap(),
        ]
        .into();
        let err = tokio::time::timeout(Duration::from_secs(5), race_connect(queue))
            .await
            .expect("failed race must resolve promptly")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }
}
