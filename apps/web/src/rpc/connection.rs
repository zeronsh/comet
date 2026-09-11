//! Browser-only bridge from the authenticated same-origin socket to `RpcClient`.
//!
//! This is intentionally not mounted by the preserved `SessionController` app.
//! A future actual `AppState` owner obtains an auth epoch, calls
//! [`connect_client`], rejects stale epochs around
//! `EngineHandle::from_connected_client`, and observes `RpcClient::watch_closed`.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use tokio::sync::{mpsc, watch};

/// Authentication and socket generation pair captured by one connection attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionEpoch {
    pub auth: u64,
    pub socket: u64,
}

/// Small caller-owned stale-result guard. Starting authentication invalidates
/// every socket; reconnecting invalidates only the previous socket.
#[derive(Default, Debug)]
pub struct ConnectionEpochs {
    auth: u64,
    socket: u64,
}

impl ConnectionEpochs {
    pub fn begin_auth(&mut self) -> ConnectionEpoch {
        self.auth = self.auth.wrapping_add(1);
        self.socket = self.socket.wrapping_add(1);
        self.current()
    }

    pub fn begin_socket(&mut self) -> ConnectionEpoch {
        self.socket = self.socket.wrapping_add(1);
        self.current()
    }

    pub fn current(&self) -> ConnectionEpoch {
        ConnectionEpoch {
            auth: self.auth,
            socket: self.socket,
        }
    }

    pub fn is_current(&self, epoch: ConnectionEpoch) -> bool {
        self.current() == epoch
    }
}

const CHANNEL_CAPACITY: usize = 2;
const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_QUEUE: usize = 128;
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUFFERED: u32 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketState {
    Connecting,
    Open,
    Closed,
}

#[derive(Clone, Copy)]
pub(crate) struct Signal {
    pub(crate) state: SocketState,
    pub(crate) sequence: u64,
}

pub(crate) struct Inbox {
    frames: VecDeque<String>,
    bytes: usize,
    closed: bool,
}

impl Inbox {
    pub(crate) fn push(&mut self, frame: String) -> bool {
        if self.closed
            || self.frames.len() >= MAX_QUEUE
            || self.bytes + frame.len() > MAX_QUEUED_BYTES
        {
            self.closed = true;
            self.frames.clear();
            self.bytes = 0;
            return false;
        }
        self.bytes += frame.len();
        self.frames.push_back(frame);
        true
    }

    fn pop(&mut self) -> Option<String> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.len();
        Some(frame)
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.frames.clear();
        self.bytes = 0;
    }
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            closed: false,
        }
    }
}

pub(crate) fn signal(sender: &watch::Sender<Signal>, state: SocketState) {
    let sequence = sender.borrow().sequence.wrapping_add(1);
    sender.send_replace(Signal { state, sequence });
}

pub(crate) trait SocketSink {
    fn send(&self, text: &str) -> Result<(), ()>;
}

pub(crate) async fn pump<S: SocketSink>(
    socket: S,
    inbox: Rc<RefCell<Inbox>>,
    mut signal_rx: watch::Receiver<Signal>,
    mut outbound: mpsc::Receiver<String>,
    inbound: mpsc::Sender<String>,
) {
    'pump: loop {
        let state = signal_rx.borrow().state;
        match state {
            SocketState::Closed => break,
            SocketState::Connecting => {
                if signal_rx.changed().await.is_err() {
                    break;
                }
            }
            SocketState::Open => {
                // Do not hold a RefCell borrow across an await: message/close
                // callbacks must always be able to update this queue.
                let queued = { inbox.borrow_mut().pop() };
                if let Some(frame) = queued {
                    // Reserve capacity before moving the frame. An ordinary
                    // signal (another frame or open event) must not turn a
                    // cancelled `send(frame)` into a dropped server response.
                    let permit = loop {
                        tokio::select! {
                            permit = inbound.reserve() => match permit {
                                Ok(permit) => break permit,
                                Err(_) => break 'pump,
                            },
                            changed = signal_rx.changed() => {
                                if changed.is_err() || signal_rx.borrow().state == SocketState::Closed {
                                    break 'pump;
                                }
                            },
                        }
                    };
                    permit.send(frame);
                    continue;
                }
                tokio::select! {
                    frame = outbound.recv() => match frame {
                        Some(frame) if socket.send(&frame).is_ok() => {},
                        Some(_) | None => break,
                    },
                    changed = signal_rx.changed() => if changed.is_err() { break; },
                }
            }
        }
    }
    drop(inbound);
    drop(socket);
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use std::{
        cell::RefCell,
        future::Future,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll},
    };

    use tokio::sync::{mpsc, oneshot, watch};
    use wasm_bindgen::{JsCast, closure::Closure};
    use web_sys::{BinaryType, Event, MessageEvent, WebSocket, Window};
    use zeron_rpc::{
        RpcClient,
        device_frame::{
            DeviceFrameHeader, ECHO_DEADLINE_MS, ECHO_KIND, PING_INTERVAL_MS, PING_TEXT, PONG_TEXT,
            RPC_KIND, SILENCE_LEASE_MS, decode_device_frame, encode_device_frame,
        },
    };

    use super::{
        CHANNEL_CAPACITY, Inbox, MAX_BUFFERED, MAX_FRAME, Signal, SocketSink, SocketState, pump,
        signal,
    };

    const HANDSHAKE_TIMEOUT_MS: i32 = 30_000;

    #[derive(Default)]
    struct Liveness {
        last_rx: f64,
        last_echo: f64,
    }

    impl Liveness {
        fn connected() -> Self {
            let now = js_sys::Date::now();
            Self {
                last_rx: now,
                last_echo: now,
            }
        }

        fn received(&mut self) {
            self.last_rx = js_sys::Date::now();
        }

        fn echoed(&mut self) {
            let now = js_sys::Date::now();
            self.last_rx = now;
            self.last_echo = now;
        }
    }

    /// Copy bytes out of WASM shared memory before passing them through the
    /// WebSocket WebIDL boundary. Chromium rejects shared `ArrayBufferView`s.
    fn send_binary(socket: &WebSocket, bytes: &[u8]) -> Result<(), ()> {
        let copy = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        copy.copy_from(bytes);
        socket
            .send_with_array_buffer(&copy.buffer())
            .map_err(|_| ())
    }

    struct BrowserHeartbeat {
        window: Window,
        id: i32,
        _callback: Closure<dyn FnMut()>,
    }

    impl BrowserHeartbeat {
        fn new(
            socket: WebSocket,
            signal_tx: watch::Sender<Signal>,
            liveness: Rc<RefCell<Liveness>>,
        ) -> Result<Self, String> {
            let window = web_sys::window().ok_or("Browser window unavailable")?;
            let callback = Closure::new(move || {
                if socket.ready_state() != WebSocket::OPEN {
                    return;
                }
                let now = js_sys::Date::now();
                let stale = {
                    let liveness = liveness.borrow();
                    now - liveness.last_rx > f64::from(SILENCE_LEASE_MS)
                        || now - liveness.last_echo > f64::from(ECHO_DEADLINE_MS)
                };
                if stale {
                    let _ = socket.close();
                    signal(&signal_tx, SocketState::Closed);
                    return;
                }
                if socket.send_with_str(PING_TEXT).is_err() {
                    let _ = socket.close();
                    signal(&signal_tx, SocketState::Closed);
                    return;
                }
                let echo_sent =
                    encode_device_frame(&DeviceFrameHeader::new(ECHO_KIND, ECHO_KIND), &[])
                        .is_ok_and(|frame| send_binary(&socket, &frame).is_ok());
                if !echo_sent {
                    let _ = socket.close();
                    signal(&signal_tx, SocketState::Closed);
                }
            });
            let id = window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    callback.as_ref().unchecked_ref(),
                    PING_INTERVAL_MS,
                )
                .map_err(|_| "Cannot start RPC liveness timer")?;
            Ok(Self {
                window,
                id,
                _callback: callback,
            })
        }
    }

    impl Drop for BrowserHeartbeat {
        fn drop(&mut self) {
            self.window.clear_interval_with_handle(self.id);
        }
    }

    struct BrowserSocket {
        socket: WebSocket,
        _heartbeat: BrowserHeartbeat,
        _open: Closure<dyn FnMut(Event)>,
        _message: Closure<dyn FnMut(MessageEvent)>,
        _close: Closure<dyn FnMut(Event)>,
        _error: Closure<dyn FnMut(Event)>,
    }

    impl SocketSink for BrowserSocket {
        fn send(&self, text: &str) -> Result<(), ()> {
            if self.socket.ready_state() != WebSocket::OPEN
                || self
                    .socket
                    .buffered_amount()
                    .saturating_add(text.len() as u32)
                    > MAX_BUFFERED
            {
                return Err(());
            }
            let frame =
                encode_device_frame(&DeviceFrameHeader::new(RPC_KIND, RPC_KIND), text.as_bytes())
                    .map_err(|_| ())?;
            if self
                .socket
                .buffered_amount()
                .saturating_add(frame.len() as u32)
                > MAX_BUFFERED
            {
                return Err(());
            }
            send_binary(&self.socket, &frame)
        }
    }

    impl Drop for BrowserSocket {
        fn drop(&mut self) {
            self.socket.set_onopen(None);
            self.socket.set_onmessage(None);
            self.socket.set_onclose(None);
            self.socket.set_onerror(None);
            let _ = self.socket.close();
        }
    }

    struct BrowserTimeout {
        receiver: oneshot::Receiver<()>,
        window: Window,
        id: i32,
        _callback: Closure<dyn FnMut()>,
    }

    impl BrowserTimeout {
        fn new(milliseconds: i32) -> Result<Self, String> {
            let window = web_sys::window().ok_or("Browser window unavailable")?;
            let (sender, receiver) = oneshot::channel();
            let mut sender = Some(sender);
            let callback = Closure::wrap(Box::new(move || {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(());
                }
            }) as Box<dyn FnMut()>);
            let id = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.as_ref().unchecked_ref(),
                    milliseconds,
                )
                .map_err(|_| "Cannot start RPC handshake timeout")?;
            Ok(Self {
                receiver,
                window,
                id,
                _callback: callback,
            })
        }
    }

    impl Future for BrowserTimeout {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            Pin::new(&mut self.receiver).poll(cx).map(|_| ())
        }
    }

    impl Drop for BrowserTimeout {
        fn drop(&mut self) {
            self.window.clear_timeout_with_handle(self.id);
        }
    }

    async fn wait_for_open(signal_rx: &mut watch::Receiver<Signal>) -> Result<(), String> {
        let mut timeout = BrowserTimeout::new(HANDSHAKE_TIMEOUT_MS)?;
        loop {
            match signal_rx.borrow().state {
                SocketState::Open => return Ok(()),
                SocketState::Closed => return Err("RPC socket closed during handshake".into()),
                SocketState::Connecting => {}
            }
            tokio::select! {
                _ = &mut timeout => return Err("RPC handshake timed out".into()),
                changed = signal_rx.changed() => if changed.is_err() {
                    return Err("RPC socket lifecycle ended during handshake".into());
                },
            }
        }
    }

    fn same_origin_rpc_url(device_id: &str) -> Result<String, String> {
        if device_id.is_empty()
            || !device_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("Invalid remote device id".into());
        }
        let location = web_sys::window()
            .ok_or("Browser window unavailable")?
            .location();
        let protocol = match location
            .protocol()
            .map_err(|_| "Cannot read origin")?
            .as_str()
        {
            "http:" => "ws",
            "https:" => "wss",
            _ => return Err("HTTP origin required".into()),
        };
        let host = location.host().map_err(|_| "Cannot read host")?;
        Ok(format!(
            "{protocol}://{host}/api/browser/device/{device_id}/ws"
        ))
    }

    /// An opened same-origin DeviceRoom socket backed by the portable typed client.
    /// Cookie credentials are supplied by the browser; this type never stores a
    /// gateway token and never reconnects or replays a write.
    pub struct ConnectedClient {
        client: RpcClient,
        url: String,
        epoch: super::ConnectionEpoch,
    }

    impl ConnectedClient {
        pub fn client(&self) -> &RpcClient {
            &self.client
        }
        pub fn url(&self) -> &str {
            &self.url
        }
        pub fn epoch(&self) -> super::ConnectionEpoch {
            self.epoch
        }
        pub fn into_client(self) -> RpcClient {
            self.client
        }
    }

    /// Open the selected remote device at `/api/browser/device/:deviceId/ws` using
    /// the browser's same-origin HttpOnly cookie. Binary frames are explicitly
    /// requested as `ArrayBuffer`; the adapter sends typed RPC only inside the
    /// DeviceRoom envelope and closes on relay, malformed, or liveness failure.
    pub async fn connect_client(
        epoch: super::ConnectionEpoch,
        device_id: &str,
    ) -> Result<ConnectedClient, String> {
        let url = same_origin_rpc_url(device_id)?;
        let socket = WebSocket::new(&url).map_err(|_| "Cannot open RPC socket")?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let inbox = Rc::new(RefCell::new(Inbox::default()));
        let liveness = Rc::new(RefCell::new(Liveness::connected()));
        let (signal_tx, mut signal_rx) = watch::channel(Signal {
            state: SocketState::Connecting,
            sequence: 0,
        });

        let open_signal = signal_tx.clone();
        let open_liveness = liveness.clone();
        let open = Closure::new(move |_: Event| {
            *open_liveness.borrow_mut() = Liveness::connected();
            signal(&open_signal, SocketState::Open);
        });
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));

        let message_inbox = inbox.clone();
        let message_signal = signal_tx.clone();
        let message_socket = socket.clone();
        let message_liveness = liveness.clone();
        let message = Closure::new(move |event: MessageEvent| {
            let accepted = if let Some(text) = event.data().as_string() {
                if text == PONG_TEXT {
                    message_liveness.borrow_mut().received();
                    true
                } else {
                    false
                }
            } else {
                event
                    .data()
                    .dyn_into::<js_sys::ArrayBuffer>()
                    .ok()
                    .map(|buffer| js_sys::Uint8Array::new(&buffer).to_vec())
                    .filter(|bytes| bytes.len() <= MAX_FRAME)
                    .and_then(|bytes| decode_device_frame(&bytes).ok())
                    .is_some_and(|(header, payload)| {
                        if header.k == ECHO_KIND {
                            message_liveness.borrow_mut().echoed();
                            true
                        } else if header.k == RPC_KIND {
                            message_liveness.borrow_mut().echoed();
                            String::from_utf8(payload)
                                .ok()
                                .filter(|text| zeron_rpc::decode_server_frame(text).is_ok())
                                .is_some_and(|text| message_inbox.borrow_mut().push(text))
                        } else {
                            false
                        }
                    })
            };
            if !accepted {
                message_inbox.borrow_mut().close();
                let _ = message_socket.close();
                signal(&message_signal, SocketState::Closed);
            } else {
                signal(&message_signal, SocketState::Open);
            }
        });
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));

        let close_inbox = inbox.clone();
        let close_signal = signal_tx.clone();
        let close = Closure::new(move |_: Event| {
            close_inbox.borrow_mut().close();
            signal(&close_signal, SocketState::Closed);
        });
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));

        let error_inbox = inbox.clone();
        let error_signal = signal_tx.clone();
        let error = Closure::new(move |_: Event| {
            error_inbox.borrow_mut().close();
            signal(&error_signal, SocketState::Closed);
        });
        socket.set_onerror(Some(error.as_ref().unchecked_ref()));

        let heartbeat = BrowserHeartbeat::new(socket.clone(), signal_tx, liveness)?;
        let (transport_out, outbound) = mpsc::channel(CHANNEL_CAPACITY);

        let (inbound_tx, inbound) = mpsc::channel(CHANNEL_CAPACITY);
        let client = RpcClient::new_with_transport(
            transport_out,
            inbound,
            pump(
                BrowserSocket {
                    socket,
                    _heartbeat: heartbeat,
                    _open: open,
                    _message: message,
                    _close: close,
                    _error: error,
                },
                inbox,
                signal_rx.clone(),
                outbound,
                inbound_tx,
            ),
        );
        wait_for_open(&mut signal_rx).await?;
        Ok(ConnectedClient { client, url, epoch })
    }
}

#[cfg(target_arch = "wasm32")]
pub use browser::{ConnectedClient, connect_client};
