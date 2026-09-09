//! Bounded, bidirectional streams over either a socket or an ordered DataChannel.
//! Each stream has an independent receive window, so a stalled response does
//! not stall other requests. END is a half-close; CANCEL aborts both directions.
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const CHUNK: usize = 8192;
const WINDOW: usize = 65536;
const MAX_STREAMS: usize = 64;
const OPEN: u8 = 1;
const DATA: u8 = 2;
const END: u8 = 3;
const CANCEL: u8 = 4;
const WS_OPEN: u8 = 5;
const WS_DATA: u8 = 6;
const WS_CLOSE: u8 = 7;
const CREDIT: u8 = 8;
const READY: u8 = 9;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;
#[async_trait::async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self, service: &str) -> anyhow::Result<BoxIo>;
}
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, bytes: &[u8]) -> anyhow::Result<()>;
    async fn receive(&self) -> anyhow::Result<Vec<u8>>;
}

pub struct SocketTransport<R, W> {
    read: tokio::sync::Mutex<R>,
    write: tokio::sync::Mutex<W>,
}
impl<R, W> SocketTransport<R, W> {
    pub fn new(read: R, write: W) -> Self {
        Self {
            read: tokio::sync::Mutex::new(read),
            write: tokio::sync::Mutex::new(write),
        }
    }
}
#[async_trait::async_trait]
impl<R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin + Send + 'static> Transport
    for SocketTransport<R, W>
{
    async fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.write.lock().await;
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(bytes).await?;
        Ok(())
    }
    async fn receive(&self) -> anyhow::Result<Vec<u8>> {
        let mut reader = self.read.lock().await;
        let size = reader.read_u32().await? as usize;
        anyhow::ensure!(
            (5..=CHUNK + 5).contains(&size),
            "invalid preview frame size"
        );
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).await?;
        Ok(bytes)
    }
}
struct Frame {
    kind: u8,
    id: u32,
    data: Vec<u8>,
}
impl Frame {
    fn new(kind: u8, id: u32, data: Vec<u8>) -> Self {
        Self { kind, id, data }
    }
    fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(5 + self.data.len());
        bytes.push(self.kind);
        bytes.extend(self.id.to_be_bytes());
        bytes.extend(self.data);
        bytes
    }
    fn decode(bytes: Vec<u8>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (5..=CHUNK + 5).contains(&bytes.len()),
            "invalid preview frame size"
        );
        Ok(Self {
            kind: bytes[0],
            id: u32::from_be_bytes(bytes[1..5].try_into()?),
            data: bytes[5..].to_vec(),
        })
    }
}
struct Slot {
    incoming: mpsc::Sender<Frame>,
    credit: Arc<Semaphore>,
    cancel: CancellationToken,
    ready: Mutex<Option<oneshot::Sender<bool>>>,
    receive_credit: AtomicUsize,
}
struct Inner {
    slots: Mutex<HashMap<u32, Arc<Slot>>>,
    outgoing: mpsc::Sender<Frame>,
    ids: AtomicU32,
    parity: u32,
    stop: CancellationToken,
}
#[derive(Clone)]
pub struct Mux(Arc<Inner>);
impl Mux {
    pub fn start(
        transport: Arc<dyn Transport>,
        connector: Arc<dyn Connector>,
        initiator: bool,
        stop: CancellationToken,
    ) -> Self {
        let (outgoing, mut output) = mpsc::channel::<Frame>(64);
        let parity = if initiator { 1 } else { 0 };
        let mux = Self(Arc::new(Inner {
            slots: Mutex::new(HashMap::new()),
            outgoing,
            ids: AtomicU32::new(if initiator { 1 } else { 2 }),
            parity,
            stop,
        }));
        let writer = transport.clone();
        let cancel = mux.0.stop.clone();
        tokio::spawn(async move {
            tokio::select! { _ = cancel.cancelled() => {}, _ = async {
                while let Some(frame) = output.recv().await { if writer.send(&frame.encode()).await.is_err() { break; } }
            } => {} }
            cancel.cancel();
        });
        let reader = mux.clone();
        tokio::spawn(async move {
            tokio::select! { _ = reader.0.stop.cancelled() => {}, _ = async {
                loop {
                    let frame = match transport.receive().await.and_then(Frame::decode) { Ok(f) => f, Err(_) => break };
                    if reader.dispatch(frame, &connector).await.is_err() { break; }
                }
            } => {} }
            reader.close();
        });
        mux
    }
    pub fn close(&self) {
        self.0.stop.cancel();
        for (_, slot) in self.0.slots.lock().unwrap().drain() {
            slot.cancel.cancel();
        }
    }
    pub async fn closed(&self) {
        self.0.stop.cancelled().await;
    }
    pub fn is_closed(&self) -> bool {
        self.0.stop.is_cancelled()
    }
    async fn send(&self, frame: Frame) -> anyhow::Result<()> {
        tokio::select! { _ = self.0.stop.cancelled() => anyhow::bail!("preview connection closed"), result = self.0.outgoing.send(frame) => { result?; Ok(()) } }
    }
    fn stream(
        &self,
        id: u32,
        websocket: bool,
    ) -> anyhow::Result<(Stream, oneshot::Receiver<bool>)> {
        let mut slots = self.0.slots.lock().unwrap();
        anyhow::ensure!(
            slots.len() < MAX_STREAMS && !slots.contains_key(&id),
            "preview stream limit reached"
        );
        let (app, bridge) = tokio::io::duplex(WINDOW);
        let (incoming, mut input) = mpsc::channel::<Frame>(WINDOW + 2);
        let (ready, result) = oneshot::channel();
        let slot = Arc::new(Slot {
            incoming,
            credit: Arc::new(Semaphore::new(WINDOW)),
            cancel: self.0.stop.child_token(),
            ready: Mutex::new(Some(ready)),
            receive_credit: AtomicUsize::new(WINDOW),
        });
        slots.insert(id, slot.clone());
        let mux = self.clone();
        let cancel = slot.cancel.clone();
        let app_cancel = cancel.clone();
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(bridge);
            let sending = async {
                let mut bytes = vec![0; CHUNK];
                loop {
                    let length = read.read(&mut bytes).await?;
                    if length == 0 {
                        mux.send(Frame::new(
                            if websocket { WS_CLOSE } else { END },
                            id,
                            Vec::new(),
                        ))
                        .await?;
                        break;
                    }
                    let permit = slot.credit.acquire_many(length as u32).await?;
                    permit.forget();
                    mux.send(Frame::new(
                        if websocket { WS_DATA } else { DATA },
                        id,
                        bytes[..length].to_vec(),
                    ))
                    .await?;
                }
                anyhow::Ok(())
            };
            let receiving = async {
                while let Some(frame) = input.recv().await {
                    if matches!(frame.kind, END | WS_CLOSE) {
                        write.shutdown().await?;
                        return anyhow::Ok(());
                    }
                    write.write_all(&frame.data).await?;
                    slot.receive_credit
                        .fetch_add(frame.data.len(), Ordering::Relaxed);
                    mux.send(Frame::new(
                        CREDIT,
                        id,
                        (frame.data.len() as u32).to_be_bytes().to_vec(),
                    ))
                    .await?;
                }
                anyhow::bail!("preview stream closed")
            };
            let graceful = tokio::select! { _ = cancel.cancelled() => false, result = async { tokio::try_join!(sending, receiving) } => result.is_ok() };
            mux.0.slots.lock().unwrap().remove(&id);
            // Queue cancellation with backpressure; closing the connection also
            // interrupts this send. No unbounded task/frame queue on stream drop.
            if !graceful {
                let _ = mux.send(Frame::new(CANCEL, id, Vec::new())).await;
            }
        });
        Ok((
            Stream {
                io: app,
                cancel: app_cancel,
                read_eof: false,
                write_closed: false,
            },
            result,
        ))
    }
    pub async fn open(&self, service: &str, websocket: bool) -> anyhow::Result<Stream> {
        anyhow::ensure!(
            !service.is_empty() && service.len() <= 128,
            "invalid preview service id"
        );
        let id = self.0.ids.fetch_add(2, Ordering::Relaxed);
        anyhow::ensure!(id < u32::MAX - 2, "preview stream ids exhausted");
        let (stream, ready) = self.stream(id, websocket)?;
        self.send(Frame::new(
            if websocket { WS_OPEN } else { OPEN },
            id,
            service.as_bytes().to_vec(),
        ))
        .await?;
        anyhow::ensure!(
            tokio::time::timeout(std::time::Duration::from_secs(10), ready).await??,
            "preview service unavailable"
        );
        Ok(stream)
    }
    async fn dispatch(&self, frame: Frame, connector: &Arc<dyn Connector>) -> anyhow::Result<()> {
        if matches!(frame.kind, OPEN | WS_OPEN) {
            anyhow::ensure!(
                frame.id != 0
                    && frame.id % 2 != self.0.parity
                    && (1..=128).contains(&frame.data.len()),
                "invalid preview OPEN"
            );
            let service = String::from_utf8(frame.data)?;
            let Ok((mut stream, _)) = self.stream(frame.id, frame.kind == WS_OPEN) else {
                return self.send(Frame::new(CANCEL, frame.id, Vec::new())).await;
            };
            let mux = self.clone();
            let connector = connector.clone();
            let id = frame.id;
            tokio::spawn(async move {
                let cancel = stream.cancel.clone();
                tokio::select! { _ = cancel.cancelled() => {}, _ = async {
                    if let Ok(Ok(mut socket)) = tokio::time::timeout(std::time::Duration::from_secs(5), connector.connect(&service)).await {
                        if mux.send(Frame::new(READY, id, Vec::new())).await.is_ok() { let _ = tokio::io::copy_bidirectional(&mut stream, &mut socket).await; }
                    }
                } => {} }
            });
            return Ok(());
        }
        let slot = self.0.slots.lock().unwrap().get(&frame.id).cloned();
        let Some(slot) = slot else {
            return Ok(());
        }; // late frames after cancellation
        match frame.kind {
            READY => {
                if let Some(ready) = slot.ready.lock().unwrap().take() {
                    let _ = ready.send(true);
                }
            }
            CANCEL => {
                slot.cancel.cancel();
                if let Some(ready) = slot.ready.lock().unwrap().take() {
                    let _ = ready.send(false);
                }
            }
            CREDIT => {
                anyhow::ensure!(frame.data.len() == 4, "invalid preview credit");
                let count = u32::from_be_bytes(frame.data.try_into().unwrap()) as usize;
                anyhow::ensure!(
                    count > 0
                        && count <= WINDOW
                        && slot.credit.available_permits() + count <= WINDOW,
                    "invalid preview receive window"
                );
                slot.credit.add_permits(count);
            }
            DATA | WS_DATA | END | WS_CLOSE => {
                // The peer cannot queue more than its advertised window. Tiny
                // frames are bounded too: abusive streams are cancelled.
                if matches!(frame.kind, DATA | WS_DATA) {
                    anyhow::ensure!(
                        !frame.data.is_empty()
                            && slot
                                .receive_credit
                                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n
                                    .checked_sub(frame.data.len()))
                                .is_ok(),
                        "preview receive window exceeded"
                    );
                }
                if slot.incoming.try_send(frame).is_err() {
                    slot.cancel.cancel();
                }
            }
            _ => anyhow::bail!("unknown preview frame"),
        }
        Ok(())
    }
}

pub struct Stream {
    io: DuplexStream,
    cancel: CancellationToken,
    read_eof: bool,
    write_closed: bool,
}
impl Drop for Stream {
    fn drop(&mut self) {
        if !(self.read_eof && self.write_closed) {
            self.cancel.cancel();
        }
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let remaining = buf.remaining();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && remaining > 0 && buf.filled().len() == before {
            self.read_eof = true;
        }
        result
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.io).poll_shutdown(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.write_closed = true;
        }
        result
    }
}

pub fn local(connector: Arc<dyn Connector>, stop: CancellationToken) -> Mux {
    // An actual local socket keeps the framing/flow-control path identical to P2P.
    #[cfg(unix)]
    let (a, b) = tokio::net::UnixStream::pair().expect("local preview socket pair");
    #[cfg(not(unix))]
    let (a, b) = tokio::io::duplex(WINDOW);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let client = Mux::start(
        Arc::new(SocketTransport::new(ar, aw)),
        connector.clone(),
        true,
        stop.child_token(),
    );
    Mux::start(
        Arc::new(SocketTransport::new(br, bw)),
        connector,
        false,
        stop.child_token(),
    );
    client
}
