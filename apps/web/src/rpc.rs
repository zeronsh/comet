//! Browser transport only. Wire envelopes and method names live in zeron-rpc.

#[cfg(target_arch = "wasm32")]
pub mod connection;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{Event, MessageEvent, WebSocket};
use zeron_rpc::{ClientFrame, ServerFrame};

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_QUEUE: usize = 128;
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUFFERED: u32 = 256 * 1024;

pub enum EventKind {
    Open,
    Frame(ServerFrame),
    Closed,
}

#[derive(Default)]
struct Inbox {
    events: VecDeque<(EventKind, usize)>,
    bytes: usize,
    closed: bool,
}
impl Inbox {
    fn push(&mut self, event: EventKind, bytes: usize) -> bool {
        if self.closed {
            return false;
        }
        if self.events.len() >= MAX_QUEUE || self.bytes + bytes > MAX_QUEUED_BYTES {
            self.close();
            return false;
        }
        self.bytes += bytes;
        self.events.push_back((event, bytes));
        true
    }
    fn close(&mut self) {
        self.closed = true;
        self.events.clear();
        self.bytes = 0;
        self.events.push_back((EventKind::Closed, 0));
    }
}

pub struct BrowserRpc {
    socket: WebSocket,
    inbox: Rc<RefCell<Inbox>>,
    _open: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(Event)>,
    _error: Closure<dyn FnMut(Event)>,
}
impl BrowserRpc {
    pub fn connect(notify: Rc<dyn Fn()>) -> Result<Self, String> {
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
        let socket = WebSocket::new(&format!("{protocol}://{host}/api/rpc"))
            .map_err(|_| "Cannot open RPC socket")?;
        let inbox = Rc::new(RefCell::new(Inbox::default()));
        let queue = inbox.clone();
        let wake = notify.clone();
        let open = Closure::new(move |_: Event| {
            queue.borrow_mut().push(EventKind::Open, 0);
            wake();
        });
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));
        let queue = inbox.clone();
        let wake = notify.clone();
        let ws = socket.clone();
        let message = Closure::new(move |event: MessageEvent| {
            let frame = event
                .data()
                .as_string()
                .filter(|s| s.len() <= MAX_FRAME)
                .and_then(|s| decode_frame(&s).ok().map(|f| (f, s.len())));
            let accepted = match frame {
                Some((frame, bytes)) => queue.borrow_mut().push(EventKind::Frame(frame), bytes),
                None => {
                    queue.borrow_mut().close();
                    false
                }
            };
            if !accepted {
                let _ = ws.close();
            }
            wake();
        });
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        let queue = inbox.clone();
        let wake = notify.clone();
        let close = Closure::new(move |_: Event| {
            queue.borrow_mut().close();
            wake();
        });
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));
        let queue = inbox.clone();
        let error = Closure::new(move |_: Event| {
            queue.borrow_mut().close();
            notify();
        });
        socket.set_onerror(Some(error.as_ref().unchecked_ref()));
        Ok(Self {
            socket,
            inbox,
            _open: open,
            _message: message,
            _close: close,
            _error: error,
        })
    }
    pub fn drain(&self) -> Vec<EventKind> {
        let mut inbox = self.inbox.borrow_mut();
        inbox.bytes = 0;
        inbox.events.drain(..).map(|(event, _)| event).collect()
    }
    /// No offline buffer. A send failure must not be retried as a mutation.
    pub fn send(&self, frame: &ClientFrame) -> Result<(), String> {
        if self.socket.ready_state() != WebSocket::OPEN || self.inbox.borrow().closed {
            return Err("Disconnected; nothing sent".into());
        }
        let text = serde_json::to_string(frame).map_err(|_| "Cannot encode request")?;
        if text.len() > MAX_BUFFERED as usize || self.socket.buffered_amount() > MAX_BUFFERED {
            return Err("RPC backpressure; nothing sent".into());
        }
        self.socket
            .send_with_str(&text)
            .map_err(|_| "RPC send failed; outcome unknown".into())
    }
    pub fn cancel(&self, id: u64) {
        let _ = self.send(&ClientFrame {
            id,
            method: None,
            params: serde_json::Value::Null,
            cancel: true,
        });
    }
}
impl Drop for BrowserRpc {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        let _ = self.socket.close();
    }
}

fn decode_frame(text: &str) -> Result<ServerFrame, String> {
    zeron_rpc::decode_server_frame(text)
}

/// Uses only the browser's same-origin HttpOnly cookie; never stores the token.
pub async fn session_request(
    method: &str,
    token: Option<String>,
    abort: &web_sys::AbortController,
) -> Result<bool, String> {
    use wasm_bindgen_futures::JsFuture;
    let options = web_sys::RequestInit::new();
    options.set_method(method);
    options.set_credentials(web_sys::RequestCredentials::SameOrigin);
    options.set_signal(Some(&abort.signal()));
    if let Some(token) = token {
        let body = serde_json::json!({"token": token}).to_string();
        options.set_body(&wasm_bindgen::JsValue::from_str(&body));
    }
    let request = web_sys::Request::new_with_str_and_init("/api/session", &options)
        .map_err(|_| "Cannot create session request")?;
    request
        .headers()
        .set("Content-Type", "application/json")
        .map_err(|_| "Cannot set request header")?;
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|_| "Session request failed")?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| "Invalid session response")?;
    if response.status() == 401 {
        return Ok(false);
    }
    if !response.ok() {
        return Err("Session request rejected".into());
    }
    if method == "DELETE" {
        return Ok(false);
    }
    let text = JsFuture::from(
        response
            .text()
            .map_err(|_| "Cannot read session response")?,
    )
    .await
    .map_err(|_| "Cannot read session response")?
    .as_string()
    .ok_or("Invalid session response")?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "Invalid session response")?;
    Ok(value
        .get("authenticated")
        .and_then(|v| v.as_bool())
        .unwrap_or(method == "POST"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_envelopes_and_null_success() {
        assert_eq!(
            decode_frame(r#"{"id":1,"ok":null}"#).unwrap().ok,
            Some(serde_json::Value::Null)
        );
        assert!(decode_frame(r#"{"id":1,"ok":{},"done":true}"#).is_err());
        assert!(decode_frame(r#"{"id":1,"err":null}"#).is_err());
        assert!(decode_frame(r#"{"id":1,"done":true}"#).unwrap().done);
    }
    #[test]
    fn bounded_queue_fails_closed() {
        let mut inbox = Inbox::default();
        for _ in 0..MAX_QUEUE {
            assert!(inbox.push(EventKind::Open, 0));
        }
        assert!(!inbox.push(EventKind::Open, 0));
        assert!(inbox.closed);
        assert_eq!(inbox.events.len(), 1);
        assert!(matches!(inbox.events[0].0, EventKind::Closed));
        assert!(!inbox.push(EventKind::Open, 0));
    }
}
