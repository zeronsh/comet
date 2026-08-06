//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring comet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use comet_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use comet_engine::{Engine, EngineConfig, EngineRuntime, rpc::AuthRpc};
use comet_proto::{AuthState, Chat, ChatIndicator, Device, HarnessId, Session, Space};
use comet_rpc::{RpcClient, RpcError, RpcReply, RpcService, connect_ws, memory_client, methods};

// ---------------------------------------------------------------------------
// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.comet-native`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// WorkOS client id for production authentication.
    pub workos_client_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    boot_task: tokio::task::JoinHandle<()>,
    refresh_task: tokio::task::JoinHandle<()>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        self.boot_task.abort();
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
        self.refresh_task.abort();
    }
}

#[derive(Clone)]
enum DeferredEngineState {
    Waiting,
    Ready(Arc<dyn RpcService>),
    Failed(String),
}

/// Serves AuthRpc immediately, then holds all data RPC calls until the signed-in
/// user's identity-scoped engine is assembled. Existing UI subscriptions remain
/// pending and attach to the real service without reconnecting.
struct DeferredEngineRpc {
    auth: AuthRpc,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
}

#[async_trait]
impl RpcService for DeferredEngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if AuthRpc::handles(method) {
            return self.auth.handle(method, params).await;
        }

        let mut state = self.state.clone();
        loop {
            let current = { state.borrow().clone() };
            match current {
                DeferredEngineState::Waiting => {}
                DeferredEngineState::Ready(service) => {
                    return service.handle(method, params).await;
                }
                DeferredEngineState::Failed(message) => return Err(RpcError::Failed(message)),
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: RpcClient,
    url: String,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; nothing to tear down.
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        let url = format!("ws://127.0.0.1:{}", config.ipc_port);
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", config.ipc_port)),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            tracing::info!(%url, "engine daemon detected; connecting");
            match connect_ws(&url).await {
                Ok(client) => {
                    return Ok(EngineHandle {
                        inner: Arc::new(RemoteEngine { client, url }),
                    });
                }
                // Something is on the port but it is not an engine (or it is
                // wedged). Fall through and embed: a stranger holding 27654
                // should cost other viewports, not this window.
                Err(err) => tracing::warn!(%url, error = %err, "not an engine; embedding instead"),
            }
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            org_id: config.org_id,
            workos_client_id: config.workos_client_id,
        };
        let auth = Engine::build_auth(&engine_config).await;
        let refresh_task = auth.spawn_refresh_loop();
        let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let service: Arc<dyn RpcService> = Arc::new(DeferredEngineRpc {
            auth: AuthRpc::new(auth.clone()),
            state: state_rx,
        });
        let client = memory_client(service.clone());

        // Serve the same service on the IPC port so a terminal viewport can
        // attach to this window's engine with no setup. Deliberately the
        // *deferred* service, not the assembled one: a viewport that connects
        // before sign-in gets AuthRpc (so it can show its own gate) and its
        // data subscriptions wait exactly as this window's do.
        //
        // Best-effort — losing the bind race with another engine costs other
        // viewports, not this one.
        let ipc_task = match comet_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(None));
        let runtime_for_boot = runtime.clone();
        let boot_task = tokio::spawn(async move {
            let mut auth_state = auth.watch_state();
            while !auth_state.borrow().is_signed_in() {
                if auth_state.changed().await.is_err() {
                    state_tx.send_replace(DeferredEngineState::Failed(
                        "authentication state closed before sign-in".into(),
                    ));
                    return;
                }
            }

            match Engine::assemble_runtime(&engine_config, auth).await {
                Ok(engine_runtime) => {
                    let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
                    *runtime_for_boot.lock().await = Some(engine_runtime);
                    state_tx.send_replace(DeferredEngineState::Ready(service));
                }
                Err(err) => {
                    tracing::error!(error = %err, "embedded engine assembly failed");
                    state_tx.send_replace(DeferredEngineState::Failed(format!("{err:#}")));
                }
            }
        });
        Ok(EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                boot_task,
                refresh_task,
                ipc_task,
                client,
            }),
        })
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `comet_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use comet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    parse_auth_state, project_label, sort_active, sort_chats, sort_spaces, sort_tabs,
};

// ---------------------------------------------------------------------------
// Org gate (pure)
// ---------------------------------------------------------------------------

/// One org membership row (tolerant local mirror of the engine's ListOrgs
/// reply — `{orgs: [{id, organizationId, name}]}`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRow {
    pub organization_id: String,
    pub name: String,
}

/// Parse a ListOrgs reply tolerantly (accepts a bare array too).
pub fn parse_orgs(value: &serde_json::Value) -> Vec<OrgRow> {
    let list = value.get("orgs").unwrap_or(value);
    serde_json::from_value(list.clone()).unwrap_or_default()
}

/// Workspace names must be non-empty (trimmed) and reasonably short.
pub fn org_name_valid(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 64
}

/// Memberships sorted by name (case-insensitive), deduped by organization id.
pub fn sort_memberships(mut orgs: Vec<OrgRow>) -> Vec<OrgRow> {
    orgs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    orgs.dedup_by(|a, b| a.organization_id == b.organization_id);
    orgs
}

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// The space whose tabs fill the main area. Healed by [`Self::apply_spaces`]
    /// when the row vanishes; selecting a chat implies its space.
    pub selected_space: Option<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// First chats / spaces watch frame has landed — device-local state that
    /// prunes against the doc (open tabs, the sidebar space filter) must not
    /// judge by the empty pre-sync lists.
    pub chats_synced: bool,
    pub spaces_synced: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<comet_update::UpdateStatus>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    engine: Option<EngineHandle>,
    watch_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_space: None,
            selected_chat: None,
            transcript: Vec::new(),
            echoes: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: None,
            engine: None,
            watch_tasks: Vec::new(),
            transcript_task: None,
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
        }
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.transcript.clear();
            self.transcript_task = None;
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        // Heal a vanished selection (space deleted elsewhere): fall back to the
        // first space; its chats died with it, so a matching chat selection is
        // healed by the accompanying chats frame (`apply_chats`).
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.spaces.first().map(|s| s.id.clone());
        }
        // First frame with no selection yet: pick the first space so the shell
        // never renders an empty main area while spaces exist.
        if self.selected_space.is_none() {
            self.selected_space = self.spaces.first().map(|s| s.id.clone());
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: comet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) {
        self.devices = devices;
    }

    pub fn apply_update(&mut self, status: comet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&comet_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        comet_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        Ok(())
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// Spaces in display order — case-insensitive alphabetical, the order
    /// both space selectors (sidebar filter, composer picker) list rows in.
    /// Ties break on id so the order is stable across renders.
    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// The "@ device" tag for a space (" · offline" appended when the host's
    /// heartbeat is stale) — shared by the space pickers' rows, the sidebar
    /// filter trigger, and the composer's space chip. Returns `(tag, offline)`.
    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline = !self.device_online(&space.device_id, now);
        let device = self.device_name(&space.device_id).unwrap_or("Unknown device");
        let tag = if offline {
            format!("@ {device} · offline")
        } else {
            format!("@ {device}")
        };
        (tag, offline)
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list).
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE space,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| {
                c.space_id
                    .as_deref()
                    .is_some_and(|id| self.space_row(id).is_some())
            })
            .map(|c| (display_status(c, self.session_for(&c.id), now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row.
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection, self.auth.as_ref())
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.connection = ConnectionStatus::Ready;
        self.engine = Some(handle.clone());
        self.watch_tasks = vec![
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
            // Auth frames parse tolerantly — engine and proto tags differ today.
            spawn_watch(
                cx,
                handle.clone(),
                methods::AUTH_STATUS,
                AppState::apply_auth_value,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ];
        // Re-subscribe the transcript if a chat was already selected (reconnect path).
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its space; `select_chat(None)` (the new-session
            // canvas) stays within the current space.
            if let Some(space_id) = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.space_id.clone())
            {
                self.selected_space = Some(space_id);
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone()) {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    /// Select a space; the caller (shell) decides which chat to land on.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_space == space_id {
            return;
        }
        self.selected_space = space_id;
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    /// Window-focus liveness sweep: ask the engine to probe every open room
    /// (workspace + chat docs). Fire-and-forget; each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so spamming is harmless.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.engine.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({});
            if let Err(err) = handle.client().call(methods::PROBE_SYNC, params).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }
}

/// Chats watch. Boot selection is the shell's job (it lands on the first
/// restored open tab, device-local state this entity can't see); this task
/// only pumps frames.
fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(error = %err, "chats watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: Vec<Chat> = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed chats frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                state.apply_chats(parsed);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(method, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(method, error = %err, "watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: T = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(method, error = %err, "dropping malformed watch frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                apply(state, parsed);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.local_device_id = Some(id);
                cx.notify();
            })
            .ok();
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop — a return here freezes the transcript
        // with no banner and no heal short of an app restart (this watch and
        // its engine-side room are the ONLY transcript delivery path). The
        // task itself is dropped by select_chat/apply_chats when the chat is
        // deselected or deleted, so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use comet_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `comet_proto::view`.
    use comet_proto::{SessionStatus, UserProfile};

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn production_bootstrap_requires_sign_in_before_opening_engine_data() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        let mut auth = handle
            .client()
            .subscribe(methods::AUTH_STATUS, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            parse_auth_state(&auth.recv().await.unwrap()),
            Some(AuthState::SignedOut)
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                handle
                    .client()
                    .call(methods::LIST_HARNESSES, serde_json::json!({})),
            )
            .await
            .is_err(),
            "data RPC must wait behind the production sign-in gate"
        );
        assert!(
            !dir.path().join("orgs/dev-org/dev-user").exists(),
            "production boot must not create dev-user data"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `comet headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-space chat (idle included) — chats of
        // unknown spaces stay hidden. Completed ("old") outranks idle ("new").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = comet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(comet_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            comet_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: comet_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        let user = UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        };
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into()), None),
            GatePhase::Failed("boom".into())
        );
        // Unknown auth (pre-M4) gates nothing.
        assert_eq!(gate_phase(&ConnectionStatus::Ready, None), GatePhase::Ready);
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, Some(&AuthState::SignedOut)),
            GatePhase::SignIn
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(&AuthState::SignedIn {
                    user: user.clone(),
                    org_id: None
                })
            ),
            GatePhase::Ready
        );
        // No org yet → org gate.
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(&AuthState::NeedsOrganization { user })
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn auth_frames_parse_both_wire_shapes() {
        // Proto shape.
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        // Engine shape (`_tag`, PascalCase, orgId).
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "orgId": "org-1",
        });
        let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(org_id.as_deref(), Some("org-1"));
        let needs = serde_json::json!({
            "_tag": "NeedsOrganization",
            "user": { "id": "u1", "email": "w@example.com", "name": "W" },
        });
        assert!(matches!(
            parse_auth_state(&needs),
            Some(AuthState::NeedsOrganization { .. })
        ));
        // Garbage → None (frame dropped, not a crash).
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/comet")), "comet");
        assert_eq!(project_label(Some("/home/w/dev/comet/")), "comet");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/comet")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/comet")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["comet", "zed", "No project"]);
        let comet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(comet_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_comet_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("comet/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · comet/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn org_gate_reducers() {
        assert!(org_name_valid("Acme"));
        assert!(org_name_valid("  padded  "));
        assert!(!org_name_valid(""));
        assert!(!org_name_valid("   "));
        assert!(!org_name_valid(&"x".repeat(65)));

        let rows = parse_orgs(&serde_json::json!({ "orgs": [
            { "id": "m2", "organizationId": "o2", "name": "beta" },
            { "id": "m1", "organizationId": "o1", "name": "Alpha" },
            { "id": "m3", "organizationId": "o1", "name": "Alpha" },
        ]}));
        assert_eq!(rows.len(), 3);
        let sorted = sort_memberships(rows);
        let names: Vec<&str> = sorted.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            ["Alpha", "beta"],
            "case-insensitive sort + dedupe by org id"
        );
        // Bare-array replies parse too; garbage yields empty.
        assert_eq!(
            parse_orgs(&serde_json::json!([{ "id": "m", "organizationId": "o", "name": "n" }]))
                .len(),
            1
        );
        assert!(parse_orgs(&serde_json::json!("nope")).is_empty());
    }
}
