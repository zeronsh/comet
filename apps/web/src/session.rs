//! Cookie-authenticated engine session. Reconnects recreate reads, never writes.
use crate::rpc::{BrowserRpc, EventKind};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
};
use wasm_bindgen::{JsCast, closure::Closure};
use zeron_doc::{
    QueuedMessage, SessionCommandPayload, SessionMessageEntry, TranscriptFrame, TranscriptUpdate,
};
use zeron_proto::{
    Chat, ChatConfig, Connectivity, ContextUsage, Device, EngineInfo, HarnessId, Model, RunRequest,
    SandboxLevel, Session, SessionStatus, Space, UserInputAnswer, WorkspaceScope, capabilities,
};
use zeron_rpc::{ClientFrame, ServerFrame, methods};

const MAX_CALLS: usize = 32;
const CALL_TIMEOUT_MS: f64 = 30_000.0;
const MAX_DRAFT_BYTES: usize = 64 * 1024;
const MAX_DRAFTS: usize = 64;
// Shared by persisted chat config and each turn; recovery must retain the same policy.
const BROWSER_SANDBOX: SandboxLevel = SandboxLevel::ReadOnly;

#[cfg(test)]
#[test]
fn browser_run_and_persisted_config_policy_is_readonly() {
    assert_eq!(BROWSER_SANDBOX, SandboxLevel::ReadOnly);
    assert_eq!(
        serde_json::to_value(BROWSER_SANDBOX).unwrap(),
        json!("read-only")
    );
}

const TURN_POLICY_NOTICE: &str = "Sending while a turn is active and answering input requests are disabled in the browser: the current turn's inherited execution policy cannot be verified. Wait for it to finish or interrupt it. The queue is read-only.";

#[derive(Clone, Debug, PartialEq)]
pub enum AuthState {
    Checking,
    SignedOut,
    SigningIn,
    SignedIn,
}
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}
#[derive(Clone, Debug, PartialEq)]
pub enum WriteOutcome {
    Pending,
    Accepted,
    UnknownOutcome,
    Rejected(String),
}

#[derive(Clone)]
pub struct SessionSnapshot {
    pub auth: AuthState,
    pub connection: ConnectionState,
    pub engine: Option<EngineInfo>,
    pub ready: bool,
    pub chats: Vec<Chat>,
    pub spaces: Vec<Space>,
    pub devices: Vec<Device>,
    pub sessions: Vec<Session>,
    pub connectivity: Option<Connectivity>,
    pub harnesses: Vec<Value>,
    pub models: Vec<Model>,
    pub selected_chat: Option<String>,
    pub selected_harness: Option<HarnessId>,
    pub selected_model: Option<String>,
    pub transcript: Vec<SessionMessageEntry>,
    pub transcript_ready: bool,
    pub context_usage: Option<ContextUsage>,
    pub queue: Vec<QueuedMessage>,
    pub draft: String,
    /// While Some, the rejected edit remains UI-owned: never replace the input
    /// from `draft`. A successful set_draft clears this flag.
    pub draft_error: Option<String>,
    pub can_send: bool,
    pub busy: bool,
    pub can_interrupt: bool,
    pub can_create_chat: bool,
    pub can_respond_input: bool,
    pub policy_notice: String,
    pub can_resolve_unknown_outcome: bool,
    pub unknown_outcome_warning: Option<String>,
    pub outcome: Option<WriteOutcome>,
    pub error: Option<String>,
}
impl Default for SessionSnapshot {
    fn default() -> Self {
        Self {
            auth: AuthState::Checking,
            connection: ConnectionState::Disconnected,
            engine: None,
            ready: false,
            chats: vec![],
            spaces: vec![],
            devices: vec![],
            sessions: vec![],
            connectivity: None,
            harnesses: vec![],
            models: vec![],
            selected_chat: None,
            selected_harness: None,
            selected_model: None,
            transcript: vec![],
            transcript_ready: false,
            context_usage: None,
            queue: vec![],
            draft: String::new(),
            draft_error: None,
            can_send: false,
            busy: false,
            can_interrupt: false,
            can_create_chat: false,
            can_respond_input: false,
            policy_notice: TURN_POLICY_NOTICE.into(),
            can_resolve_unknown_outcome: false,
            unknown_outcome_warning: None,
            outcome: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug)]
enum Read {
    Info,
    Ready,
    Chats,
    Spaces,
    Devices,
    Sessions,
    Connectivity,
    Harnesses,
    Models(HarnessId),
    Transcript(u64),
    Queue(u64),
}
impl Read {
    fn method(&self) -> &'static str {
        match self {
            Self::Info => methods::ENGINE_INFO,
            Self::Ready => methods::ENGINE_READY,
            Self::Chats => methods::WATCH_CHATS,
            Self::Spaces => methods::WATCH_SPACES,
            Self::Devices => methods::WATCH_DEVICES,
            Self::Sessions => methods::WATCH_SESSIONS,
            Self::Connectivity => methods::WATCH_CONNECTIVITY,
            Self::Harnesses => methods::LIST_HARNESSES,
            Self::Models(_) => methods::LIST_MODELS,
            Self::Transcript(_) => methods::WATCH_DOC_MESSAGES,
            Self::Queue(_) => methods::WATCH_QUEUE,
        }
    }
    fn stream(&self) -> bool {
        matches!(
            self,
            Self::Chats
                | Self::Spaces
                | Self::Devices
                | Self::Sessions
                | Self::Connectivity
                | Self::Transcript(_)
                | Self::Queue(_)
        )
    }
    fn chat_generation(&self) -> Option<u64> {
        match self {
            Self::Transcript(g) | Self::Queue(g) => Some(*g),
            _ => None,
        }
    }
}
#[derive(Clone)]
struct Write {
    chat: Option<String>,
    draft: Option<String>,
    message_id: Option<String>,
    created_chat: Option<String>,
}
#[derive(Clone)]
enum CallKind {
    Read(Read),
    Write(Write),
}
struct Call {
    kind: CallKind,
    deadline: Option<f64>,
}

#[derive(Default)]
struct ReviewFreshness {
    since_generation: u64,
    engine: Option<EngineInfo>,
    chats: Option<u64>,
    sessions: Option<u64>,
    transcript: Option<(u64, u64)>,
    queue: Option<(u64, u64)>,
}

struct State {
    view: SessionSnapshot,
    rpc: Option<BrowserRpc>,
    calls: BTreeMap<u64, Call>,
    next_id: u64,
    generation: u64,
    chat_generation: u64,
    auth_generation: u64,
    abort: Option<web_sys::AbortController>,
    auth_deadline: Option<f64>,
    connect_deadline: Option<f64>,
    retry_at: Option<f64>,
    retries: u32,
    drafts: HashMap<String, String>,
    draft_errors: HashMap<String, String>,
    outcomes: HashMap<String, WriteOutcome>,
    unknown_writes: HashMap<String, Write>,
    unknown_review: HashMap<String, ReviewFreshness>,
    sessions_ready: bool,
    queue_ready: bool,
    transcript_recoveries: u8,
    started: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            view: SessionSnapshot::default(),
            rpc: None,
            calls: BTreeMap::new(),
            next_id: 1,
            generation: 0,
            chat_generation: 0,
            auth_generation: 0,
            abort: None,
            auth_deadline: None,
            connect_deadline: None,
            retry_at: None,
            retries: 0,
            drafts: HashMap::new(),
            draft_errors: HashMap::new(),
            outcomes: HashMap::new(),
            unknown_writes: HashMap::new(),
            unknown_review: HashMap::new(),
            sessions_ready: false,
            queue_ready: false,
            transcript_recoveries: 0,
            started: false,
        }
    }
}
impl State {
    fn set_draft(&mut self, text: String) -> Result<(), String> {
        let chat = self
            .view
            .selected_chat
            .clone()
            .ok_or("Choose a chat before editing")?;
        // Only accepted drafts and the current unaccepted chat need error markers.
        // Rejected text stays in the UI input, not in another unbounded map here.
        self.draft_errors
            .retain(|id, _| self.drafts.contains_key(id) || id == &chat);
        let error = if text.len() > MAX_DRAFT_BYTES {
            Some("Draft exceeds 64 KiB. Your edit remains in the input; shorten it before sending.")
        } else if !text.is_empty()
            && !self.drafts.contains_key(&chat)
            && self.drafts.len() >= MAX_DRAFTS
        {
            Some(
                "Draft storage is full (64 chats). Your edit remains in the input; clear another chat's draft before sending.",
            )
        } else {
            None
        };
        if let Some(error) = error {
            self.draft_errors.insert(chat, error.into());
            self.view.error = Some(error.into());
            return Err(error.into());
        }
        if let Some(previous) = self.draft_errors.remove(&chat) {
            // A later transport/request error must survive correction of the draft.
            if self.view.error.as_ref() == Some(&previous) {
                self.view.error = None;
            }
        }
        if text.is_empty() {
            self.drafts.remove(&chat);
        } else {
            self.drafts.insert(chat, text);
        }
        Ok(())
    }
    fn unknown_key(&self) -> Option<&str> {
        self.view
            .selected_chat
            .as_deref()
            .filter(|id| self.unknown_writes.contains_key(*id))
            .or_else(|| self.unknown_writes.contains_key("").then_some(""))
    }
    fn can_review(&self, key: &str) -> bool {
        let (Some(write), Some(fresh)) =
            (self.unknown_writes.get(key), self.unknown_review.get(key))
        else {
            return false;
        };
        let v = &self.view;
        let identity_matches = matches!((&v.engine, &fresh.engine), (Some(current), Some(original))
            if current.device_id == original.device_id && current.workspace_scope == original.workspace_scope);
        if !identity_matches
            || self.generation <= fresh.since_generation
            || !v.ready
            || v.auth != AuthState::SignedIn
            || v.connection != ConnectionState::Connected
            || self
                .calls
                .values()
                .any(|c| matches!(c.kind, CallKind::Write(_)))
        {
            return false;
        }
        if write.created_chat.is_some() {
            return fresh.chats == Some(self.generation);
        }
        let transcript_current = fresh.transcript == Some((self.generation, self.chat_generation));
        let queue_current = fresh.queue == Some((self.generation, self.chat_generation));
        v.selected_chat.as_deref() == Some(key)
            && v.transcript_ready
            && self.sessions_ready
            && transcript_current
            && fresh.sessions == Some(self.generation)
            && (write.draft.is_none()
                || write.message_id.is_some()
                || (self.queue_ready && queue_current))
    }
    fn resolve_unknown_outcome(&mut self, confirmed_inspected: bool) -> Result<(), String> {
        if !confirmed_inspected {
            return Err("Confirm that the operation may already have happened and you inspected the current authoritative state".into());
        }
        let key = self
            .unknown_key()
            .ok_or("No unknown outcome to review")?
            .to_owned();
        if !self.can_review(&key) {
            return Err(
                "Reconnect and inspect fresh authoritative snapshots before confirming review"
                    .into(),
            );
        }
        self.unknown_writes.remove(&key);
        self.unknown_review.remove(&key);
        self.outcomes.remove(&key);
        self.view.error = Some("Unknown outcome reviewed by you, not verified as successful or failed. Draft retained. No operation was sent or retried.".into());
        Ok(())
    }
    fn record_review_read(&mut self, read: &Read, reset: bool) {
        for (key, fresh) in &mut self.unknown_review {
            if self.generation <= fresh.since_generation {
                continue;
            }
            match read {
                Read::Chats => fresh.chats = Some(self.generation),
                Read::Sessions => fresh.sessions = Some(self.generation),
                Read::Transcript(g)
                    if *g == self.chat_generation
                        && reset
                        && self.view.transcript_ready
                        && self.view.selected_chat.as_ref() == Some(key) =>
                {
                    fresh.transcript = Some((self.generation, *g))
                }
                Read::Queue(g)
                    if *g == self.chat_generation
                        && self.queue_ready
                        && self.view.selected_chat.as_ref() == Some(key) =>
                {
                    fresh.queue = Some((self.generation, *g))
                }
                _ => {}
            }
        }
    }

    fn recompute(&mut self) {
        let review_key = self.unknown_key().map(str::to_owned);
        let can_review = review_key
            .as_deref()
            .is_some_and(|key| self.can_review(key));
        let warning = review_key.as_ref().and_then(|key| self.unknown_writes.get(key)).map(|w| {
            let operation = if w.created_chat.is_some() { "Chat creation" }
                else if w.message_id.is_some() { "Sending this message" }
                else if w.draft.is_some() { "Queueing this message" } else { "The control command" };
            format!("{operation} may already have happened. Reconnect and inspect the fresh chat list, transcript, live status and queue before explicitly confirming review. Confirmation only removes the block; it does not send or retry anything.")
        });
        let v = &mut self.view;
        v.draft_error = v
            .selected_chat
            .as_ref()
            .and_then(|id| self.draft_errors.get(id))
            .cloned();
        v.can_resolve_unknown_outcome = can_review;
        v.unknown_outcome_warning = warning;
        v.can_respond_input = false;
        v.draft = v
            .selected_chat
            .as_ref()
            .and_then(|id| self.drafts.get(id))
            .cloned()
            .unwrap_or_default();
        v.outcome = v
            .selected_chat
            .as_ref()
            .and_then(|id| self.outcomes.get(id))
            .cloned()
            .or_else(|| self.outcomes.get("").cloned())
            // An RPC receipt (QueueCommand returns only commandId) is not a run
            // status. Keep it internally for draft/create reconciliation, never
            // as a persistent success badge over a failed or unfinished turn.
            .filter(|outcome| !matches!(outcome, WriteOutcome::Accepted));
        if review_key.is_some() {
            v.outcome = Some(WriteOutcome::UnknownOutcome);
        }
        let online =
            v.auth == AuthState::SignedIn && v.connection == ConnectionState::Connected && v.ready;
        let local = v
            .engine
            .as_ref()
            .is_some_and(|e| e.workspace_scope == WorkspaceScope::Local);
        let chat = v
            .chats
            .iter()
            .find(|c| Some(&c.id) == v.selected_chat.as_ref());
        let local_chat = chat.is_some_and(|c| {
            !c.archived
                && v.engine
                    .as_ref()
                    .is_some_and(|e| e.device_id == c.device_id)
                && v.spaces
                    .iter()
                    .any(|s| Some(&s.id) == c.space_id.as_ref() && s.device_id == c.device_id)
        });
        v.busy = v.sessions.iter().any(|s| {
            Some(&s.chat_id) == v.selected_chat.as_ref()
                && matches!(
                    s.status,
                    SessionStatus::Working | SessionStatus::AwaitingInput
                )
        });
        let catalog = v
            .selected_harness
            .is_some_and(|h| harness_enabled(&v.harnesses, h))
            && v.selected_model
                .as_ref()
                .is_some_and(|id| v.models.iter().any(|m| &m.id == id));
        let pending = self
            .calls
            .values()
            .any(|c| matches!(c.kind, CallKind::Write(_)));
        let uncertain = matches!(v.outcome, Some(WriteOutcome::UnknownOutcome));
        let chat_ready = online && local && local_chat && v.transcript_ready && self.sessions_ready;
        v.can_send = chat_ready
            && catalog
            && !pending
            && !uncertain
            && !v.draft.trim().is_empty()
            && v.draft.len() <= MAX_DRAFT_BYTES
            && v.draft_error.is_none()
            && !v.busy;
        v.can_interrupt = chat_ready && v.busy && !pending && !uncertain;
        v.can_create_chat = online
            && local
            && catalog
            && !pending
            && !self.unknown_writes.contains_key("")
            && v.spaces.iter().any(|s| {
                v.engine
                    .as_ref()
                    .is_some_and(|e| e.device_id == s.device_id)
            });
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        kind: CallKind,
        now: f64,
    ) -> Result<u64, String> {
        if self.calls.len() >= MAX_CALLS {
            return Err("Too many outstanding RPC calls".into());
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("RPC request ids exhausted")?;
        self.rpc
            .as_ref()
            .ok_or("Disconnected; nothing sent")?
            .send(&ClientFrame {
                id,
                method: Some(method.into()),
                params,
                cancel: false,
            })?;
        self.calls.insert(
            id,
            Call {
                kind,
                deadline: Some(now + CALL_TIMEOUT_MS),
            },
        );
        Ok(id)
    }
    fn read(&mut self, read: Read, now: f64) -> Result<(), String> {
        let params = match &read {
            Read::Models(h) => json!({"harness": h}),
            Read::Transcript(_) | Read::Queue(_) => json!({"chatId": self.view.selected_chat}),
            _ => json!({}),
        };
        self.request(read.method(), params, CallKind::Read(read), now)
            .map(|_| ())
    }
    fn cancel_chat(&mut self) {
        self.chat_generation += 1;
        let ids: Vec<_> = self
            .calls
            .iter()
            .filter_map(|(&id, c)| {
                matches!(&c.kind, CallKind::Read(r) if r.chat_generation().is_some()).then_some(id)
            })
            .collect();
        for id in ids {
            self.calls.remove(&id);
            if let Some(rpc) = &self.rpc {
                rpc.cancel(id);
            }
        }
        self.view.transcript.clear();
        self.view.transcript_ready = false;
        self.view.context_usage = None;
        self.view.queue.clear();
        self.queue_ready = false;
    }
    fn subscribe_chat(&mut self, now: f64) -> Result<(), String> {
        self.cancel_chat();
        if self.view.selected_chat.is_some() && self.view.ready {
            self.read(Read::Transcript(self.chat_generation), now)?;
            if self
                .view
                .engine
                .as_ref()
                .is_some_and(|e| e.supports(capabilities::MESSAGE_QUEUE_V1))
            {
                self.read(Read::Queue(self.chat_generation), now)?;
            }
        }
        Ok(())
    }
    fn mark_unknown(&mut self, write: &Write) {
        self.outcomes.insert(
            write.chat.clone().unwrap_or_default(),
            WriteOutcome::UnknownOutcome,
        );
        self.unknown_writes
            .entry(write.chat.clone().unwrap_or_default())
            .or_insert_with(|| write.clone());
        self.unknown_review
            .entry(write.chat.clone().unwrap_or_default())
            .or_insert_with(|| ReviewFreshness {
                since_generation: self.generation,
                engine: self.view.engine.clone(),
                ..Default::default()
            });
        self.view.error = Some("Connection lost or request timed out: write outcome unknown. Check the transcript/queue before sending anything again. Draft retained; nothing will be retried.".into());
    }
    fn disconnect(&mut self, now: f64, retry: bool) {
        for (_, call) in std::mem::take(&mut self.calls) {
            if let CallKind::Write(write) = call.kind {
                self.mark_unknown(&write);
            }
        }
        self.generation += 1;
        self.rpc = None;
        self.connect_deadline = None;
        self.view.connection = ConnectionState::Disconnected;
        self.view.ready = false;
        self.sessions_ready = false;
        self.view.harnesses.clear();
        self.view.models.clear();
        self.cancel_chat();
        self.retry_at = if retry && self.view.auth == AuthState::SignedIn {
            self.retries = (self.retries + 1).min(5);
            Some(now + (1u32 << self.retries) as f64 * 1000.0)
        } else {
            None
        };
        self.recompute();
    }
    fn receive(&mut self, generation: u64, frame: ServerFrame, now: f64) -> Result<(), String> {
        if generation != self.generation {
            return Ok(());
        }
        let Some(call) = self.calls.get(&frame.id) else {
            return Ok(());
        };
        let kind = call.kind.clone();
        if matches!(&kind, CallKind::Read(r) if r.chat_generation().is_some_and(|g| g != self.chat_generation))
        {
            self.calls.remove(&frame.id);
            return Ok(());
        }
        if let Some(error) = frame.err {
            self.calls.remove(&frame.id);
            match kind {
                CallKind::Write(w) => {
                    self.outcomes.insert(
                        w.chat.unwrap_or_default(),
                        WriteOutcome::Rejected(error.clone()),
                    );
                }
                CallKind::Read(Read::Transcript(_)) => {
                    self.view.transcript_ready = false;
                    self.view.transcript.clear();
                }
                CallKind::Read(Read::Queue(_)) => self.queue_ready = false,
                CallKind::Read(Read::Sessions) => self.sessions_ready = false,
                // A failed identity, workspace or catalog feed cannot authorize
                // writes using its previous snapshot. Explicit reconnect reloads it.
                CallKind::Read(_) => self.view.ready = false,
            }
            self.view.error = Some(error);
            return Ok(());
        }
        match kind {
            CallKind::Write(write) => {
                if frame.ok.is_none() {
                    return Err("Unexpected write reply; outcome unknown".into());
                }
                self.calls.remove(&frame.id);
                self.accept_write(&write);
                if let Some(chat) = write.created_chat {
                    self.view.selected_chat = Some(chat);
                    self.subscribe_chat(now)?;
                }
            }
            CallKind::Read(read) => {
                if frame.done {
                    self.calls.remove(&frame.id);
                    return Err(format!(
                        "{} stream ended; refreshing connection",
                        read.method()
                    ));
                }
                let value = if read.stream() { frame.item } else { frame.ok }
                    .ok_or("Unexpected RPC response kind")?;
                if read.stream() {
                    self.calls.get_mut(&frame.id).unwrap().deadline = None;
                } else {
                    self.calls.remove(&frame.id);
                }
                self.apply_read(read, value, now)?;
            }
        }
        Ok(())
    }
    fn accept_write(&mut self, write: &Write) {
        let key = write.chat.clone().unwrap_or_default();
        self.outcomes.insert(key.clone(), WriteOutcome::Accepted);
        if let Some(sent) = &write.draft {
            if self.drafts.get(&key) == Some(sent) {
                self.drafts.remove(&key);
            }
        }
        // Acknowledging an interrupt must not erase an earlier unknown send.
        if write.draft.is_some() || write.created_chat.is_some() {
            self.unknown_writes.remove(&key);
            self.unknown_review.remove(&key);
        }
    }
    fn apply_read(&mut self, read: Read, value: Value, now: f64) -> Result<(), String> {
        fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
            serde_json::from_value(value).map_err(|_| "Engine returned an invalid snapshot".into())
        }
        let observed_read = read.clone();
        let reset = value.get("reset").is_some();
        match read {
            Read::Info => {
                let info: EngineInfo = decode(value)?;
                if self.view.engine.as_ref().is_some_and(|old| {
                    old.device_id != info.device_id || old.workspace_scope != info.workspace_scope
                }) {
                    self.cancel_chat();
                    self.view.selected_chat = None;
                    self.view.selected_harness = None;
                    self.view.selected_model = None;
                    self.view.chats.clear();
                    self.view.spaces.clear();
                    self.view.devices.clear();
                    self.view.sessions.clear();
                    self.view.error = Some("Engine identity changed. Choose a chat and model explicitly before sending.".into());
                }
                self.view.engine = Some(info);
                self.read(Read::Ready, now)?;
            }
            Read::Ready => {
                if value.get("ready").and_then(Value::as_bool) != Some(true) {
                    return Err("Engine not ready".into());
                }
                self.view.ready = true;
                for read in [
                    Read::Chats,
                    Read::Spaces,
                    Read::Devices,
                    Read::Sessions,
                    Read::Connectivity,
                    Read::Harnesses,
                ] {
                    self.read(read, now)?;
                }
                self.subscribe_chat(now)?;
            }
            Read::Chats => {
                self.view.chats = decode(value)?;
                if self
                    .view
                    .selected_chat
                    .as_ref()
                    .is_some_and(|id| self.view.chats.iter().any(|c| &c.id == id))
                {
                    // The created row has now arrived; do not preserve a future
                    // deleted selection just because some earlier create succeeded.
                    if matches!(self.outcomes.get(""), Some(WriteOutcome::Accepted)) {
                        self.outcomes.remove("");
                    }
                }
                let created = self
                    .unknown_writes
                    .get("")
                    .filter(|w| {
                        w.created_chat
                            .as_ref()
                            .is_some_and(|id| self.view.chats.iter().any(|c| &c.id == id))
                    })
                    .cloned();
                if let Some(write) = created {
                    self.accept_write(&write);
                }
                if self
                    .view
                    .selected_chat
                    .as_ref()
                    .is_some_and(|id| !self.view.chats.iter().any(|c| &c.id == id))
                {
                    // A create acknowledgement may precede its workspace watch. Keep
                    // selection but leave controls gated until its real row arrives.
                    if !matches!(self.outcomes.get(""), Some(WriteOutcome::Accepted)) {
                        self.cancel_chat();
                        self.view.selected_chat = None;
                    }
                }
            }
            Read::Spaces => self.view.spaces = decode(value)?,
            Read::Devices => self.view.devices = decode(value)?,
            Read::Sessions => {
                self.view.sessions = decode(value)?;
                self.sessions_ready = true;
            }
            Read::Connectivity => self.view.connectivity = Some(decode(value)?),
            Read::Harnesses => {
                self.view.harnesses = decode(value)?;
                if let Some(h) = self
                    .view
                    .selected_harness
                    .filter(|h| harness_enabled(&self.view.harnesses, *h))
                {
                    self.read(Read::Models(h), now)?;
                }
            }
            Read::Models(h) => {
                if self.view.selected_harness == Some(h) {
                    self.view.models = decode(value)?;
                }
            }
            Read::Queue(_) => {
                self.view.queue = decode(
                    value
                        .get("items")
                        .cloned()
                        .ok_or("Invalid queue snapshot")?,
                )?;
                self.queue_ready = true;
            }
            Read::Transcript(_) => {
                let update: TranscriptUpdate = decode(value)?;
                if let Err(error) = apply_transcript(&mut self.view, update) {
                    self.view.error = Some(error);
                    self.transcript_recoveries += 1;
                    if self.transcript_recoveries > 2 {
                        return Err(
                            "Transcript repeatedly desynchronized; reconnect required".into()
                        );
                    }
                    self.subscribe_chat(now)?;
                } else if let Some(chat) = &self.view.selected_chat {
                    let adopted = self
                        .unknown_writes
                        .get(chat)
                        .filter(|w| {
                            w.message_id.as_ref().is_some_and(|id| {
                                self.view.transcript.iter().any(|entry| &entry.id == id)
                            })
                        })
                        .cloned();
                    if let Some(write) = adopted {
                        // messageId is evidence of an adopted Run, never a replay key.
                        self.accept_write(&write);
                        self.view.error =
                            Some("Previously unconfirmed send found in the transcript.".into());
                    }
                }
            }
        }
        self.record_review_read(&observed_read, reset);
        Ok(())
    }
}

fn harness_enabled(catalog: &[Value], harness: HarnessId) -> bool {
    let id = serde_json::to_value(harness).unwrap_or(Value::Null);
    harness != HarnessId::Mock
        && catalog.iter().any(|h| {
            h.get("id") == Some(&id)
                && h.get("installed").and_then(Value::as_bool) == Some(true)
                && h.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        })
}
fn apply_transcript(view: &mut SessionSnapshot, update: TranscriptUpdate) -> Result<(), String> {
    let reset = matches!(update.frame, TranscriptFrame::Reset { .. });
    let result = if !view.transcript_ready && !reset {
        Err("Transcript delta arrived before reset".into())
    } else {
        zeron_doc::apply_transcript_frame(&mut view.transcript, update.frame)
            .map_err(|e| e.to_string())
    };
    if let Err(error) = result {
        view.transcript.clear();
        view.transcript_ready = false;
        view.context_usage = None;
        return Err(format!(
            "{error}; discarding transcript and requesting a fresh snapshot"
        ));
    }
    view.transcript_ready = true;
    view.context_usage = update.context_usage;
    Ok(())
}

struct Inner {
    state: RefCell<State>,
    on_change: Rc<dyn Fn()>,
}
impl Inner {
    fn notify(&self) {
        self.state.borrow_mut().recompute();
        (self.on_change)();
    }
    fn connect(this: &Rc<Self>) {
        let now = js_sys::Date::now();
        let generation = {
            let mut s = this.state.borrow_mut();
            if s.view.auth != AuthState::SignedIn {
                return;
            }
            s.disconnect(now, false);
            s.view.connection = ConnectionState::Connecting;
            s.connect_deadline = Some(now + CALL_TIMEOUT_MS);
            s.generation
        };
        let weak = Rc::downgrade(this);
        let notify: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(inner) = weak.upgrade() {
                Self::pump(&inner, generation);
            }
        });
        match BrowserRpc::connect(notify) {
            Ok(rpc) => this.state.borrow_mut().rpc = Some(rpc),
            Err(error) => {
                let mut s = this.state.borrow_mut();
                s.view.error = Some(error);
                s.disconnect(now, true);
            }
        }
        this.notify();
    }
    fn pump(this: &Rc<Self>, generation: u64) {
        let now = js_sys::Date::now();
        {
            let mut s = this.state.borrow_mut();
            if s.generation != generation {
                return;
            }
            let events = s.rpc.as_ref().map(BrowserRpc::drain).unwrap_or_default();
            for event in events {
                let result = match event {
                    EventKind::Open => {
                        s.view.connection = ConnectionState::Connected;
                        s.connect_deadline = None;
                        s.retries = 0;
                        s.read(Read::Info, now)
                    }
                    EventKind::Frame(frame) => s.receive(generation, frame, now),
                    EventKind::Closed => {
                        s.disconnect(now, true);
                        break;
                    }
                };
                if let Err(error) = result {
                    s.view.error = Some(error);
                    s.disconnect(now, true);
                    break;
                }
            }
        }
        this.notify();
    }
    fn auth(this: &Rc<Self>, method: &'static str, token: Option<String>) {
        let abort = match web_sys::AbortController::new() {
            Ok(abort) => abort,
            Err(_) => {
                this.state.borrow_mut().view.error = Some("Cannot start session request".into());
                this.notify();
                return;
            }
        };
        let generation = {
            let mut s = this.state.borrow_mut();
            if let Some(old) = s.abort.take() {
                old.abort();
            }
            s.auth_generation += 1;
            s.auth_deadline = Some(js_sys::Date::now() + CALL_TIMEOUT_MS);
            s.abort = Some(abort.clone());
            s.view.auth = if method == "POST" {
                AuthState::SigningIn
            } else {
                AuthState::Checking
            };
            s.disconnect(js_sys::Date::now(), false);
            s.auth_generation
        };
        this.notify();
        let weak = Rc::downgrade(this);
        wasm_bindgen_futures::spawn_local(async move {
            let result = crate::rpc::session_request(method, token, &abort).await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let signed_in = {
                let mut s = inner.state.borrow_mut();
                if generation != s.auth_generation {
                    return;
                }
                s.abort = None;
                s.auth_deadline = None;
                match result {
                    Ok(true) => {
                        s.view.auth = AuthState::SignedIn;
                        true
                    }
                    Ok(false) => {
                        s.view.auth = AuthState::SignedOut;
                        false
                    }
                    Err(error) => {
                        s.view.auth = AuthState::SignedOut;
                        s.view.error = Some(error);
                        false
                    }
                }
            };
            if signed_in {
                Self::connect(&inner);
            } else {
                inner.notify();
            }
        });
    }
    fn tick(this: &Rc<Self>) {
        let now = js_sys::Date::now();
        let retry = {
            let mut s = this.state.borrow_mut();
            if s.auth_deadline.is_some_and(|d| now >= d) {
                s.auth_generation += 1;
                if let Some(abort) = s.abort.take() {
                    abort.abort();
                }
                s.auth_deadline = None;
                s.view.auth = AuthState::SignedOut;
                s.view.error = Some("Session request timed out".into());
            }
            if s.connect_deadline.is_some_and(|d| now >= d)
                || s.calls
                    .values()
                    .any(|c| c.deadline.is_some_and(|d| now >= d))
            {
                s.view.error = Some("RPC timed out; reconnecting reads only".into());
                s.disconnect(now, true);
            }
            s.retry_at.is_some_and(|d| now >= d)
        };
        // Recheck the cookie before reconnect: an expired/revoked session returns
        // to login rather than looping a rejected WebSocket handshake forever.
        if retry {
            Self::auth(this, "GET", None);
        } else {
            this.notify();
        }
    }
}

/// Own in an Rc in the UI; callbacks should capture only a weak UI entity.
/// new does no IO. start after the UI entity is fully constructed.
pub struct SessionController {
    inner: Rc<Inner>,
    timer: RefCell<Option<(i32, Closure<dyn FnMut()>)>>,
}
impl SessionController {
    pub fn new(on_change: impl Fn() + 'static) -> Self {
        Self {
            inner: Rc::new(Inner {
                state: RefCell::new(State::default()),
                on_change: Rc::new(on_change),
            }),
            timer: RefCell::new(None),
        }
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut s = self.inner.state.borrow_mut();
        s.recompute();
        s.view.clone()
    }
    pub fn start(&self) {
        if self.inner.state.borrow().started {
            return;
        }
        self.inner.state.borrow_mut().started = true;
        let weak = Rc::downgrade(&self.inner);
        let tick = Closure::new(move || {
            if let Some(inner) = weak.upgrade() {
                Inner::tick(&inner);
            }
        });
        if let Some(window) = web_sys::window() {
            match window.set_interval_with_callback_and_timeout_and_arguments_0(
                tick.as_ref().unchecked_ref(),
                1000,
            ) {
                Ok(id) => *self.timer.borrow_mut() = Some((id, tick)),
                Err(_) => {
                    self.inner.state.borrow_mut().view.error =
                        Some("Cannot start lifecycle timer".into());
                    self.inner.notify();
                    return;
                }
            }
        }
        Inner::auth(&self.inner, "GET", None);
    }
    /// Caller must clear its token input immediately; token is not kept in state.
    pub fn login(&self, token: String) {
        Inner::auth(&self.inner, "POST", Some(token));
    }
    pub fn logout(&self) {
        {
            let mut s = self.inner.state.borrow_mut();
            s.view.selected_chat = None;
            s.cancel_chat();
        }
        Inner::auth(&self.inner, "DELETE", None);
    }
    pub fn reconnect(&self) {
        Inner::auth(&self.inner, "GET", None);
    }
    pub fn select_chat(&self, chat_id: Option<String>) {
        {
            let mut s = self.inner.state.borrow_mut();
            if let Some(id) = &chat_id {
                if !s.view.chats.iter().any(|c| {
                    &c.id == id
                        && s.view
                            .engine
                            .as_ref()
                            .is_some_and(|e| c.device_id == e.device_id)
                }) {
                    s.view.error = Some("Choose an existing chat on this engine".into());
                    drop(s);
                    self.inner.notify();
                    return;
                }
            }
            s.view.selected_chat = chat_id;
            s.transcript_recoveries = 0;
            if let Err(error) = s.subscribe_chat(js_sys::Date::now()) {
                s.view.error = Some(error);
            }
        }
        self.inner.notify();
    }
    /// Use an empty model when changing harness to load its real catalog first.
    pub fn select_model(&self, harness: HarnessId, model: String) {
        {
            let mut s = self.inner.state.borrow_mut();
            if !harness_enabled(&s.view.harnesses, harness) {
                s.view.error = Some("Harness is not available on this engine".into());
            } else {
                let changed = s.view.selected_harness != Some(harness);
                s.view.selected_harness = Some(harness);
                s.view.selected_model = (!model.is_empty()).then_some(model);
                if changed || s.view.models.is_empty() {
                    s.view.models.clear();
                    let ids: Vec<_> = s
                        .calls
                        .iter()
                        .filter_map(|(&id, c)| {
                            matches!(c.kind, CallKind::Read(Read::Models(_))).then_some(id)
                        })
                        .collect();
                    for id in ids {
                        s.calls.remove(&id);
                        if let Some(rpc) = &s.rpc {
                            rpc.cancel(id);
                        }
                    }
                    if let Err(error) = s.read(Read::Models(harness), js_sys::Date::now()) {
                        s.view.error = Some(error);
                    }
                }
            }
        }
        self.inner.notify();
    }
    /// On Err, the UI retains its rejected input and must not synchronize it
    /// from snapshot.draft while draft_error is Some. No rejected text is copied
    /// into controller state; accepted drafts remain bounded to 64 x 64 KiB.
    pub fn set_draft(&self, text: String) -> Result<(), String> {
        let result = self.inner.state.borrow_mut().set_draft(text);
        self.inner.notify();
        result
    }
    /// The caller must obtain explicit user confirmation after inspecting the
    /// newly refreshed authoritative state. This never sends or retries a write.
    pub fn resolve_unknown_outcome(&self, confirmed_inspected: bool) -> Result<(), String> {
        let result = self
            .inner
            .state
            .borrow_mut()
            .resolve_unknown_outcome(confirmed_inspected);
        self.inner.notify();
        result
    }
    pub fn send(&self) -> Result<(), String> {
        let result = {
            let mut s = self.inner.state.borrow_mut();
            s.recompute();
            if let Some(error) = &s.view.draft_error {
                return Err(error.clone());
            }
            if s.view.busy {
                return Err(TURN_POLICY_NOTICE.into());
            }
            if !s.view.can_send {
                return Err(
                    "Send unavailable: check connection, readiness, model and previous outcome"
                        .into(),
                );
            }
            let chat = s
                .view
                .chats
                .iter()
                .find(|c| Some(&c.id) == s.view.selected_chat.as_ref())
                .unwrap()
                .clone();
            let text = s.view.draft.clone();
            let (method, params, message_id) = {
                let id = new_id()?;
                let space = s
                    .view
                    .spaces
                    .iter()
                    .find(|space| Some(&space.id) == chat.space_id.as_ref())
                    .ok_or("Chat space unavailable")?;
                let request = RunRequest {
                    prompt: text.clone(),
                    harness: s.view.selected_harness,
                    model: s.view.selected_model.clone(),
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: chat.cwd.clone().unwrap_or_else(|| space.path.clone()),
                    sandbox: BROWSER_SANDBOX,
                    auto_approve: false,
                    resume: None,
                    attachments: vec![],
                    worktree: None,
                };
                let command = SessionCommandPayload::Run {
                    request,
                    message_id: id.clone(),
                };
                (
                    methods::QUEUE_COMMAND,
                    json!({"chatId": chat.id, "command": command}),
                    Some(id),
                )
            };
            submit_write(
                &mut s,
                method,
                params,
                Write {
                    chat: Some(chat.id),
                    draft: Some(text),
                    message_id,
                    created_chat: None,
                },
            )
        };
        self.inner.notify();
        result
    }
    pub fn interrupt(&self) -> Result<(), String> {
        if !self.snapshot().can_interrupt {
            return Err("No live local turn to interrupt".into());
        }
        self.command(SessionCommandPayload::Interrupt {})
    }
    pub fn respond_input(
        &self,
        request_id: String,
        answers: Vec<UserInputAnswer>,
    ) -> Result<(), String> {
        let _ = (request_id, answers);
        Err(TURN_POLICY_NOTICE.into())
    }
    fn command(&self, command: SessionCommandPayload) -> Result<(), String> {
        let result = {
            let mut s = self.inner.state.borrow_mut();
            let chat = s.view.selected_chat.clone().ok_or("No chat selected")?;
            submit_write(
                &mut s,
                methods::QUEUE_COMMAND,
                json!({"chatId": chat, "command": command}),
                Write {
                    chat: Some(chat),
                    draft: None,
                    message_id: None,
                    created_chat: None,
                },
            )
        };
        self.inner.notify();
        result
    }
    pub fn create_chat(&self, space_id: String, title: String) -> Result<(), String> {
        // Existing createChat does not accept a title; the engine names the chat
        // from its first turn. Never send a second, unauthorized rename mutation.
        let _ = title;
        let result = {
            let mut s = self.inner.state.borrow_mut();
            s.recompute();
            if !s.view.can_create_chat {
                return Err(
                    "Choose an available harness/model and wait for engine readiness".into(),
                );
            }
            if !s.view.spaces.iter().any(|space| {
                space.id == space_id
                    && s.view
                        .engine
                        .as_ref()
                        .is_some_and(|e| space.device_id == e.device_id)
            }) {
                return Err("Choose an existing space on this engine".into());
            }
            if matches!(s.outcomes.get(""), Some(WriteOutcome::UnknownOutcome)) {
                return Err(
                    "Previous create outcome unknown; inspect chats before creating again".into(),
                );
            }
            let id = new_id()?;
            let config = ChatConfig {
                harness: s.view.selected_harness.unwrap(),
                model: s.view.selected_model.clone(),
                reasoning: None,
                model_options: Default::default(),
                sandbox: BROWSER_SANDBOX,
            };
            submit_write(
                &mut s,
                methods::MUTATE,
                json!({"op": "createChat", "chatId": id, "spaceId": space_id, "config": config}),
                Write {
                    chat: None,
                    draft: None,
                    message_id: None,
                    created_chat: Some(id),
                },
            )
        };
        self.inner.notify();
        result
    }
}
fn submit_write(s: &mut State, method: &str, params: Value, write: Write) -> Result<(), String> {
    let key = write.chat.clone().unwrap_or_default();
    match s.request(
        method,
        params,
        CallKind::Write(write.clone()),
        js_sys::Date::now(),
    ) {
        Ok(_) => {
            s.outcomes.insert(key, WriteOutcome::Pending);
            Ok(())
        }
        Err(error) => {
            // Conservatively retain the draft even if send() threw before delivery.
            if error.contains("outcome unknown") {
                s.mark_unknown(&write);
            }
            s.view.error = Some(error.clone());
            Err(error)
        }
    }
}
fn new_id() -> Result<String, String> {
    // Invoke the browser's cryptographic UUID source without another dependency.
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let crypto = js_sys::Reflect::get(window.as_ref(), &"crypto".into())
        .map_err(|_| "Crypto unavailable")?;
    let random = js_sys::Reflect::get(&crypto, &"randomUUID".into())
        .map_err(|_| "UUID unavailable")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "UUID unavailable")?;
    random
        .call0(&crypto)
        .map_err(|_| "UUID generation failed")?
        .as_string()
        .ok_or("Invalid UUID".into())
}
impl Drop for SessionController {
    fn drop(&mut self) {
        if let Some((id, _closure)) = self.timer.borrow_mut().take() {
            if let Some(window) = web_sys::window() {
                window.clear_interval_with_handle(id);
            }
        }
        let mut s = self.inner.state.borrow_mut();
        s.auth_generation += 1;
        if let Some(abort) = s.abort.take() {
            abort.abort();
        }
        if let Some(rpc) = &s.rpc {
            for id in s.calls.keys() {
                rpc.cancel(*id);
            }
        }
        s.calls.clear();
        s.rpc = None;
        s.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(value: Value) -> TranscriptUpdate {
        serde_json::from_value(value).unwrap()
    }
    fn reset() -> TranscriptUpdate {
        update(
            json!({"reset": [{"id":"m", "role":"assistant", "parts":[{"kind":"text", "id":"p", "text":"é"}], "createdAt":0, "deviceId":"d"}]}),
        )
    }
    #[test]
    fn unicode_reset_delta_error_and_recovery() {
        let mut view = SessionSnapshot::default();
        assert!(apply_transcript(&mut view, update(json!({"count":0}))).is_err());
        apply_transcript(&mut view, reset()).unwrap();
        apply_transcript(
            &mut view,
            update(json!({"append":[{"entry":"m","part":"p","text":"🙂","len":6}],"count":1})),
        )
        .unwrap();
        assert!(
            matches!(&view.transcript[0].parts[0], zeron_doc::MessagePart::Text { text, .. } if text == "é🙂")
        );
        assert!(
            apply_transcript(
                &mut view,
                update(json!({"append":[{"entry":"m","part":"p","text":"!","len":3}],"count":1}))
            )
            .is_err()
        );
        assert!(view.transcript.is_empty());
        assert!(!view.transcript_ready);
        apply_transcript(&mut view, reset()).unwrap();
        assert!(view.transcript_ready);
    }
    #[test]
    fn stale_socket_and_chat_frames_cannot_land() {
        let mut s = State::default();
        s.generation = 2;
        s.chat_generation = 5;
        s.calls.insert(
            1,
            Call {
                kind: CallKind::Read(Read::Transcript(4)),
                deadline: None,
            },
        );
        let frame = ServerFrame {
            id: 1,
            item: Some(json!({"reset":[]})),
            ..Default::default()
        };
        s.receive(1, frame.clone(), 0.0).unwrap();
        assert_eq!(s.calls.len(), 1);
        s.receive(2, frame, 0.0).unwrap();
        assert!(s.calls.is_empty());
        assert!(!s.view.transcript_ready);
    }
    #[test]
    fn disconnect_never_replays_and_duplicate_ack_is_ignored() {
        let mut s = State::default();
        let write = Write {
            chat: Some("c".into()),
            draft: Some("hello".into()),
            message_id: Some("m".into()),
            created_chat: None,
        };
        s.drafts.insert("c".into(), "hello".into());
        s.calls.insert(
            1,
            Call {
                kind: CallKind::Write(write),
                deadline: Some(10.0),
            },
        );
        s.disconnect(0.0, false);
        assert!(s.calls.is_empty());
        assert_eq!(s.drafts["c"], "hello");
        assert_eq!(s.outcomes["c"], WriteOutcome::UnknownOutcome);
        s.receive(
            s.generation,
            ServerFrame {
                id: 1,
                ok: Some(json!({})),
                ..Default::default()
            },
            0.0,
        )
        .unwrap();
        assert_eq!(s.outcomes["c"], WriteOutcome::UnknownOutcome);
    }
    #[test]
    fn ack_only_clears_matching_draft() {
        let mut s = State::default();
        let write = Write {
            chat: Some("c".into()),
            draft: Some("old".into()),
            message_id: None,
            created_chat: None,
        };
        s.drafts.insert("c".into(), "new".into());
        s.accept_write(&write);
        assert_eq!(s.drafts["c"], "new");
        s.drafts.insert("c".into(), "old".into());
        s.accept_write(&write);
        assert!(!s.drafts.contains_key("c"));
    }
    #[test]
    fn cancel_chat_invalidates_and_readiness_fails_closed() {
        let mut s = State::default();
        s.view.transcript_ready = true;
        s.calls.insert(
            1,
            Call {
                kind: CallKind::Read(Read::Queue(0)),
                deadline: None,
            },
        );
        s.cancel_chat();
        s.recompute();
        assert!(s.calls.is_empty());
        assert_eq!(s.chat_generation, 1);
        assert!(!s.view.transcript_ready);
        assert!(!s.view.can_send);
        assert!(!s.view.can_interrupt);
        assert!(!s.view.can_create_chat);
    }
    #[test]
    fn interrupt_ack_cannot_turn_unknown_send_into_retry() {
        let mut s = State::default();
        s.view.selected_chat = Some("c".into());
        let send = Write {
            chat: Some("c".into()),
            draft: Some("hello".into()),
            message_id: Some("m".into()),
            created_chat: None,
        };
        s.drafts.insert("c".into(), "hello".into());
        s.mark_unknown(&send);
        s.accept_write(&Write {
            chat: Some("c".into()),
            draft: None,
            message_id: None,
            created_chat: None,
        });
        s.recompute();
        assert_eq!(s.view.outcome, Some(WriteOutcome::UnknownOutcome));
        assert_eq!(s.view.draft, "hello");
        assert!(!s.view.can_send);
        s.apply_read(
            Read::Transcript(0),
            serde_json::to_value(reset()).unwrap(),
            0.0,
        )
        .unwrap();
        s.recompute();
        assert_eq!(s.outcomes["c"], WriteOutcome::Accepted);
        assert_eq!(s.view.outcome, None); // Adoption resolves uncertainty, not model success.
        assert!(s.view.draft.is_empty());
    }
    #[test]
    fn done_and_errors_do_not_leave_live_stream_state() {
        let mut s = State::default();
        s.calls.insert(
            1,
            Call {
                kind: CallKind::Read(Read::Sessions),
                deadline: None,
            },
        );
        assert!(
            s.receive(
                0,
                ServerFrame {
                    id: 1,
                    done: true,
                    ..Default::default()
                },
                0.0
            )
            .is_err()
        );
        assert!(s.calls.is_empty());
        s.view.transcript_ready = true;
        s.calls.insert(
            2,
            Call {
                kind: CallKind::Read(Read::Transcript(0)),
                deadline: None,
            },
        );
        s.receive(
            0,
            ServerFrame {
                id: 2,
                err: Some("watch failed".into()),
                ..Default::default()
            },
            0.0,
        )
        .unwrap();
        assert!(s.calls.is_empty());
        assert!(!s.view.transcript_ready);
    }
    #[test]
    fn identity_change_requires_explicit_selection() {
        let mut s = State::default();
        s.view.engine = Some(EngineInfo {
            device_id: "old".into(),
            workspace_scope: WorkspaceScope::Local,
            capabilities: vec![],
        });
        s.view.selected_chat = Some("c".into());
        s.view.selected_harness = Some(HarnessId::Codex);
        // No socket in this pure test: the following readiness read fails, after
        // applying the real identity transition and invalidating selection.
        assert!(
            s.apply_read(
                Read::Info,
                json!({"deviceId":"new","workspaceScope":"local"}),
                0.0
            )
            .is_err()
        );
        assert!(s.view.selected_chat.is_none());
        assert!(s.view.selected_harness.is_none());
        s.recompute();
        assert!(!s.view.can_send);
    }
    fn ready_state() -> State {
        let mut s = State::default();
        s.view.auth = AuthState::SignedIn;
        s.view.connection = ConnectionState::Connected;
        s.view.ready = true;
        s.view.engine = Some(EngineInfo {
            device_id: "d".into(),
            workspace_scope: WorkspaceScope::Local,
            capabilities: vec![capabilities::MESSAGE_QUEUE_V1.into()],
        });
        s.view.chats = serde_json::from_value(json!([{"id":"c","deviceId":"d","archived":false,"spaceId":"s","createdAt":"2026-01-01T00:00:00Z"}])).unwrap();
        s.view.spaces = serde_json::from_value(
            json!([{"id":"s","deviceId":"d","path":"/project","createdAt":"2026-01-01T00:00:00Z"}]),
        )
        .unwrap();
        s.view.selected_chat = Some("c".into());
        s.view.selected_harness = Some(HarnessId::Codex);
        s.view.selected_model = Some("model".into());
        s.view.harnesses = vec![json!({"id":"codex","installed":true,"enabled":true})];
        s.view.models = serde_json::from_value(json!([{"id":"model","label":"Model"}])).unwrap();
        s.view.transcript_ready = true;
        s.sessions_ready = true;
        s.queue_ready = true;
        s.drafts
            .insert("c".into(), "previous accepted draft".into());
        s.recompute();
        assert!(s.view.can_send);
        s
    }
    #[test]
    fn review_oversized_edit_rejects_and_immediate_send_cannot_use_old_draft() {
        let controller = SessionController::new(|| {});
        *controller.inner.state.borrow_mut() = ready_state();
        let result = controller.set_draft("x".repeat(MAX_DRAFT_BYTES + 1));
        assert!(result.is_err());
        assert!(!controller.snapshot().can_send);
        assert!(controller.send().is_err());
        assert!(controller.inner.state.borrow().calls.is_empty());
    }
    #[test]
    fn review_draft_map_limit_is_an_explicit_rejection() {
        let controller = SessionController::new(|| {});
        let mut s = ready_state();
        s.drafts.clear();
        for id in 0..64 {
            s.drafts.insert(format!("other-{id}"), "draft".into());
        }
        *controller.inner.state.borrow_mut() = s;
        let result = controller.set_draft("new chat edit".into());
        assert!(result.is_err());
        assert!(!controller.snapshot().can_send);
        assert!(controller.send().is_err());
        assert_eq!(controller.inner.state.borrow().drafts.len(), 64);
    }
    #[test]
    fn review_busy_send_is_unavailable_even_with_queue_capability() {
        let mut s = ready_state();
        s.view.sessions = serde_json::from_value(json!([{"chatId":"c","deviceId":"d","status":"working","updatedAt":"2026-01-01T00:00:00Z"}])).unwrap();
        s.recompute();
        assert!(s.view.busy);
        assert!(s.view.can_interrupt);
        assert!(!s.view.can_send);
    }

    fn unknown_control() -> Write {
        Write {
            chat: Some("c".into()),
            draft: None,
            message_id: None,
            created_chat: None,
        }
    }
    fn reconnect_state(s: &mut State) {
        s.disconnect(0.0, false);
        s.view.connection = ConnectionState::Connected;
        s.view.ready = true;
    }
    fn fresh_chat_state(s: &mut State) {
        s.apply_read(Read::Sessions, json!([]), 0.0).unwrap();
        s.apply_read(
            Read::Transcript(s.chat_generation),
            json!({"reset":[]}),
            0.0,
        )
        .unwrap();
    }
    #[test]
    fn review_unknown_control_requires_new_socket_fresh_reset_and_confirmation() {
        let mut s = ready_state();
        s.mark_unknown(&unknown_control());
        fresh_chat_state(&mut s); // Same-socket updates cannot authorize review.
        assert!(!s.can_review("c"));
        assert!(s.resolve_unknown_outcome(true).is_err());
        reconnect_state(&mut s);
        s.apply_read(Read::Sessions, json!([]), 0.0).unwrap();
        assert!(!s.can_review("c"));
        s.apply_read(
            Read::Transcript(s.chat_generation),
            json!({"reset":[]}),
            0.0,
        )
        .unwrap();
        assert!(s.can_review("c"));
        assert!(s.resolve_unknown_outcome(false).is_err());
        let draft = s.drafts["c"].clone();
        let next_id = s.next_id;
        s.resolve_unknown_outcome(true).unwrap();
        assert_eq!(s.drafts["c"], draft);
        assert!(s.calls.is_empty());
        assert_eq!(s.next_id, next_id);
        assert!(!s.unknown_writes.contains_key("c"));
        assert!(!s.outcomes.contains_key("c")); // Never pretend Accepted.
        s.recompute();
        assert!(s.view.outcome.is_none());
    }
    #[test]
    fn review_unknown_queue_needs_queue_snapshot_and_never_uses_text_as_evidence() {
        let mut s = ready_state();
        let write = Write {
            draft: Some(s.drafts["c"].clone()),
            ..unknown_control()
        };
        s.mark_unknown(&write);
        reconnect_state(&mut s);
        fresh_chat_state(&mut s);
        assert!(!s.can_review("c"));
        let row = QueuedMessage::new("server-assigned", write.draft.clone().unwrap(), "d");
        s.apply_read(Read::Queue(s.chat_generation), json!({"items":[row]}), 0.0)
            .unwrap();
        assert!(s.can_review("c"));
        assert!(s.unknown_writes.contains_key("c"));
        s.resolve_unknown_outcome(true).unwrap();
        assert_eq!(s.drafts.get("c"), write.draft.as_ref());
        assert!(s.calls.is_empty());
    }
    #[test]
    fn review_stale_socket_chat_delta_and_identity_cannot_authorize_confirmation() {
        let mut s = ready_state();
        s.mark_unknown(&unknown_control());
        reconnect_state(&mut s);
        s.apply_read(Read::Sessions, json!([]), 0.0).unwrap();
        let g = s.generation;
        let chat_g = s.chat_generation;
        s.calls.insert(
            1,
            Call {
                kind: CallKind::Read(Read::Transcript(chat_g)),
                deadline: None,
            },
        );
        let frame = ServerFrame {
            id: 1,
            item: Some(json!({"reset":[]})),
            ..Default::default()
        };
        s.receive(g - 1, frame.clone(), 0.0).unwrap();
        assert!(!s.can_review("c"));
        s.cancel_chat();
        s.receive(g, frame, 0.0).unwrap();
        assert!(!s.can_review("c"));
        // Even a valid delta following an old local transcript is not a fresh reset.
        s.view.transcript_ready = true;
        s.apply_read(Read::Transcript(s.chat_generation), json!({"count":0}), 0.0)
            .unwrap();
        assert!(!s.can_review("c"));
        s.apply_read(
            Read::Transcript(s.chat_generation),
            json!({"reset":[]}),
            0.0,
        )
        .unwrap();
        assert!(s.can_review("c"));
        s.view.engine.as_mut().unwrap().device_id = "different".into();
        assert!(!s.can_review("c"));
        assert!(s.resolve_unknown_outcome(true).is_err());
        s.view.engine.as_mut().unwrap().device_id = "d".into();
        s.disconnect(0.0, false);
        assert!(!s.can_review("c"));
    }
    #[test]
    fn review_unknown_create_uses_fresh_chat_registry_not_transcript() {
        let mut s = ready_state();
        s.mark_unknown(&Write {
            chat: None,
            draft: None,
            message_id: None,
            created_chat: Some("new-id".into()),
        });
        reconnect_state(&mut s);
        assert!(!s.can_review(""));
        let chats = serde_json::to_value(&s.view.chats).unwrap();
        s.apply_read(Read::Chats, chats, 0.0).unwrap();
        assert!(s.can_review(""));
        s.resolve_unknown_outcome(true).unwrap();
        assert!(s.calls.is_empty());
        assert!(s.drafts.contains_key("c"));
    }
    #[test]
    fn review_rejected_input_survives_ack_notifications_and_is_bounded() {
        let controller = SessionController::new(|| {});
        *controller.inner.state.borrow_mut() = ready_state();
        let mut input = "é".repeat(MAX_DRAFT_BYTES);
        assert!(controller.set_draft(input.clone()).is_err());
        let accepted = Write {
            draft: Some("previous accepted draft".into()),
            ..unknown_control()
        };
        controller.inner.state.borrow_mut().accept_write(&accepted);
        for _ in 0..3 {
            let snapshot = controller.snapshot();
            assert!(snapshot.draft_error.is_some());
            if snapshot.draft_error.is_none() {
                input = snapshot.draft;
            }
        }
        assert_eq!(input.len(), MAX_DRAFT_BYTES * 2);
        assert!(controller.send().is_err());
        controller.set_draft("corrected".into()).unwrap();
        assert!(controller.snapshot().draft_error.is_none());
        assert_eq!(controller.snapshot().draft, "corrected");
        let mut s = controller.inner.state.borrow_mut();
        for id in 0..200 {
            s.view.selected_chat = Some(format!("rejected-{id}"));
            assert!(s.set_draft("x".repeat(MAX_DRAFT_BYTES + 1)).is_err());
        }
        assert!(s.draft_errors.len() <= MAX_DRAFTS + 1);
        assert!(s.drafts.values().all(|text| text.len() <= MAX_DRAFT_BYTES));
    }
    #[test]
    fn corrected_or_discarded_draft_clears_only_its_error() {
        let mut s = ready_state();
        for corrected in ["corrected", ""] {
            assert!(s.set_draft("x".repeat(MAX_DRAFT_BYTES + 1)).is_err());
            s.set_draft(corrected.into()).unwrap();
            assert!(
                s.view.error.is_none(),
                "stale draft rejection remains visible"
            );
        }
        assert!(s.set_draft("x".repeat(MAX_DRAFT_BYTES + 1)).is_err());
        s.view.error = Some("RPC disconnected".into());
        s.set_draft("corrected".into()).unwrap();
        assert_eq!(s.view.error.as_deref(), Some("RPC disconnected"));
    }

    #[test]
    fn command_receipt_is_not_a_persistent_run_status() {
        let mut s = ready_state();
        s.calls.insert(
            99,
            Call {
                kind: CallKind::Write(Write {
                    chat: Some("c".into()),
                    draft: Some("hello".into()),
                    message_id: Some("m".into()),
                    created_chat: None,
                }),
                deadline: Some(10.0),
            },
        );
        s.drafts.insert("c".into(), "hello".into());
        s.drafts.insert("other".into(), "keep".into());
        s.receive(
            s.generation,
            ServerFrame {
                id: 99,
                ok: Some(json!({"commandId":"command"})),
                ..Default::default()
            },
            0.0,
        )
        .unwrap();
        s.recompute();
        assert_eq!(s.outcomes["c"], WriteOutcome::Accepted);
        assert_eq!(
            s.view.outcome, None,
            "receipt is not model execution status"
        );
        assert!(s.view.draft.is_empty());
        assert_eq!(s.drafts["other"], "keep");
        for status in ["streaming", "complete", "aborted"] {
            s.apply_read(
                Read::Transcript(s.chat_generation),
                json!({"reset":[{
                    "id":"m", "role":"assistant", "parts":[], "status":status,
                    "createdAt":0, "deviceId":"d"
                }]}),
                0.0,
            )
            .unwrap();
            s.recompute();
            assert_eq!(s.view.outcome, None);
            assert_eq!(
                serde_json::to_value(&s.view.transcript[0]).unwrap()["status"],
                status
            );
        }
        s.view.error = Some("delivery failed".into());
        for outcome in [
            WriteOutcome::Pending,
            WriteOutcome::Rejected("rejected".into()),
            WriteOutcome::UnknownOutcome,
        ] {
            s.outcomes.insert("c".into(), outcome.clone());
            s.recompute();
            assert_eq!(s.view.outcome, Some(outcome));
            assert_eq!(s.view.error.as_deref(), Some("delivery failed"));
        }
    }

    #[test]
    fn review_answers_are_disabled_without_issuing_a_call() {
        let controller = SessionController::new(|| {});
        *controller.inner.state.borrow_mut() = ready_state();
        assert!(!controller.snapshot().can_respond_input);
        assert!(
            controller
                .respond_input("request".into(), vec![])
                .unwrap_err()
                .contains("inherited execution policy")
        );
        assert!(controller.inner.state.borrow().calls.is_empty());
    }
}
