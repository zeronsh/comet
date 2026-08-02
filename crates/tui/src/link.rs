//! The engine link: one supervisor task that owns the RPC connection and all
//! subscriptions, and talks to the render loop over two channels.
//!
//! Why a supervisor and not `RpcClient` calls from the draw loop:
//!
//! - **Decoding is not the renderer's job.** Every watch frame arrives as a
//!   `serde_json::Value` carrying a *full snapshot* (the engine's watch streams
//!   are `tokio::sync::watch`, so a burst of doc commits collapses to the
//!   latest value). Deserializing into typed rows here keeps the render loop's
//!   only work layout and diffing — it never touches serde.
//! - **The daemon is a separate process.** It can be restarted under us
//!   (`comet daemon restart`, an upgrade, a crash). The supervisor notices the
//!   streams ending, reconnects with backoff, and resubscribes; the app just
//!   sees `Connection` updates and keeps its own state. Nothing in the
//!   viewport needs to know a reconnect happened.
//! - **A slow call must not stall input.** Outgoing calls are spawned, so a
//!   `QueueCommand` that takes a second cannot delay a keystroke.

use std::sync::Arc;
use std::time::Duration;

use comet_doc::SessionMessageEntry;
use comet_proto::view::ConnectionStatus;
use comet_proto::{AuthState, Chat, Device, Session, Space};
use comet_rpc::{RpcClient, methods};
use tokio::sync::mpsc;

use crate::daemon::{Attachment, DaemonConfig};

/// Everything the render loop learns about the world.
#[derive(Debug)]
pub enum Update {
    Connection(ConnectionStatus),
    /// How we reached the engine — decides what quitting means.
    Attached(Attachment),
    Auth(Box<AuthState>),
    Chats(Vec<Chat>),
    Spaces(Vec<Space>),
    Devices(Vec<Device>),
    Sessions(Vec<Session>),
    /// A transcript snapshot. Carries the chat id so a frame that raced a
    /// selection change is dropped rather than rendered under the wrong title.
    Transcript {
        chat_id: String,
        entries: Vec<SessionMessageEntry>,
    },
    /// This engine's device id — the host for spaces we create.
    LocalDevice(String),
    /// The model catalogue for a harness, answering [`Command::ListModels`].
    Models(Vec<comet_proto::Model>),
    /// A space's branches, answering [`Command::ListRefs`].
    Refs(Vec<comet_proto::RepoRef>),
    /// A drafted session became real: the chat exists and its prompt is queued.
    SessionStarted {
        chat_id: String,
    },
    /// A transient message for the status line.
    Notice(String),
    /// An optimistic send that didn't land: the app drops the echo and hands
    /// the text back to the composer rather than silently losing it.
    SendFailed {
        chat_id: String,
        message_id: String,
        error: String,
    },
}

/// Everything the render loop asks of the engine.
#[derive(Debug)]
pub enum Command {
    /// Retarget the per-chat transcript subscription (`None` unsubscribes).
    /// Only the visible chat's transcript is streamed: an idle engine should
    /// not be serializing docs nobody is looking at.
    WatchTranscript(Option<String>),
    /// Fire-and-forget call; failures surface as [`Update::Notice`].
    Call {
        method: &'static str,
        params: serde_json::Value,
        /// What to say if it fails ("Couldn't archive the session").
        context: &'static str,
    },
    /// A send, tracked so a failure can be attributed to its echo.
    Send {
        chat_id: String,
        message_id: String,
        params: serde_json::Value,
    },
    /// Fetch the model catalogue for a harness. Unlike [`Command::Call`] this
    /// one's *reply* is wanted, so it comes back as [`Update::Models`].
    ListModels { harness: comet_proto::HarnessId },
    /// Fetch a space's branches, for the ref picker.
    ListRefs {
        repo_path: String,
        target_device: Option<String>,
    },
    /// Turn a drafted session into a real one, then send its first prompt.
    ///
    /// This is one command rather than three because the steps are *ordered and
    /// dependent*: a new worktree has to exist before the chat can name it as
    /// its cwd, and the chat has to exist before its prompt can be queued.
    /// Sequencing that in the reducer would mean modelling half-finished
    /// sessions; sequencing it here means a failure at any step leaves nothing
    /// behind but a notice.
    StartSession(Box<StartSession>),
    /// Drop this connection and dial again now, skipping the backoff. What `r`
    /// does after the user has fixed whatever was wrong.
    Reconnect,
    /// Drop the connection and stop reconnecting (quit path).
    Shutdown,
}

/// Everything needed to materialize a drafted session.
#[derive(Debug)]
pub struct StartSession {
    pub chat_id: String,
    pub space_id: String,
    pub repo_path: String,
    pub target_device: Option<String>,
    pub plan: comet_proto::view::CheckoutPlan,
    pub config: Option<serde_json::Value>,
    pub message_id: String,
    /// The already-encoded `SessionCommandPayload::Run`.
    pub command: serde_json::Value,
}

pub struct EngineLink {
    pub updates: mpsc::UnboundedReceiver<Update>,
    pub commands: mpsc::UnboundedSender<Command>,
    supervisor: tokio::task::JoinHandle<()>,
}

impl EngineLink {
    /// Ask the supervisor to do something. A closed channel means the
    /// supervisor is gone, which only happens on the quit path — dropping the
    /// command is correct there.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for EngineLink {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        self.supervisor.abort();
    }
}

/// Reconnect backoff: quick first retries (a `comet daemon restart` is back in
/// well under a second) flattening to a 5s poll so a long-down engine costs
/// nothing.
const BACKOFF_MS: [u64; 6] = [200, 400, 800, 1_600, 3_200, 5_000];

/// Start the supervisor. It connects immediately (spawning a daemon if needed)
/// and keeps the connection alive for the life of the app.
pub fn spawn(config: DaemonConfig) -> EngineLink {
    let (update_tx, updates) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let supervisor = tokio::spawn(supervise(config, update_tx, command_rx));
    EngineLink {
        updates,
        commands,
        supervisor,
    }
}

async fn supervise(
    config: DaemonConfig,
    updates: mpsc::UnboundedSender<Update>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    // The transcript target survives reconnects: after the engine comes back we
    // resubscribe whatever the user is still looking at.
    let mut transcript_target: Option<String> = None;
    let mut attempt = 0usize;

    loop {
        if updates
            .send(Update::Connection(ConnectionStatus::Connecting))
            .is_err()
        {
            return; // App is gone.
        }

        match crate::daemon::connect(&config).await {
            Ok(connection) => {
                attempt = 0;
                let _ = updates.send(Update::Attached(connection.attachment.clone()));
                let _ = updates.send(Update::Connection(ConnectionStatus::Ready));
                let client = Arc::new(connection.client);
                match session(&client, &updates, &mut commands, &mut transcript_target).await {
                    SessionEnd::Shutdown => return,
                    SessionEnd::AppGone => return,
                    SessionEnd::ConnectionLost => {
                        let _ = updates.send(Update::Connection(ConnectionStatus::Failed(
                            "engine connection lost".into(),
                        )));
                    }
                }
            }
            Err(err) => {
                if updates
                    .send(Update::Connection(ConnectionStatus::Failed(format!(
                        "{err:#}"
                    ))))
                    .is_err()
                {
                    return;
                }
            }
        }

        // Backoff, but stay responsive: Shutdown ends us now and Reconnect
        // short-circuits the wait. Commands that need a connection are dropped
        // — except the transcript target, which we must remember so the
        // resubscribe after reconnect restores what the user is reading.
        let wait = Duration::from_millis(BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]);
        attempt = attempt.saturating_add(1);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => return,
                    Some(Command::Reconnect) => {
                        attempt = 0;
                        break;
                    }
                    Some(Command::WatchTranscript(target)) => transcript_target = target,
                    Some(_) => {}
                },
            }
        }
    }
}

enum SessionEnd {
    /// A stream ended or a subscribe failed — the daemon went away.
    ConnectionLost,
    /// The app asked us to stop.
    Shutdown,
    /// The app dropped its receiver.
    AppGone,
}

/// Serve one connection: subscribe everything, then pump until something breaks.
async fn session(
    client: &Arc<RpcClient>,
    updates: &mpsc::UnboundedSender<Update>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    transcript_target: &mut Option<String>,
) -> SessionEnd {
    let empty = || serde_json::json!({});

    // The engine's device id is a plain call, not a stream. Best-effort: an
    // engine that doesn't serve it yet just leaves space creation disabled.
    match client.call(methods::LOCAL_DEVICE, empty()).await {
        Ok(value) => {
            if let Some(id) = value.get("deviceId").and_then(|v| v.as_str())
                && updates.send(Update::LocalDevice(id.to_string())).is_err()
            {
                return SessionEnd::AppGone;
            }
        }
        Err(err) => tracing::debug!(error = %err, "LocalDevice unavailable"),
    }

    let (mut chats, mut spaces, mut devices, mut sessions, mut auth) = match tokio::try_join!(
        client.subscribe(methods::WATCH_CHATS, empty()),
        client.subscribe(methods::WATCH_SPACES, empty()),
        client.subscribe(methods::WATCH_DEVICES, empty()),
        client.subscribe(methods::WATCH_SESSIONS, empty()),
        client.subscribe(methods::AUTH_STATUS, empty()),
    ) {
        Ok(streams) => streams,
        Err(err) => {
            tracing::warn!(error = %err, "engine subscribe failed");
            return SessionEnd::ConnectionLost;
        }
    };

    // Resubscribe the visible transcript (first connect: usually None; after a
    // reconnect: whatever the user was reading).
    let mut transcript = match transcript_target.clone() {
        Some(chat_id) => open_transcript(client, chat_id).await,
        None => None,
    };

    loop {
        tokio::select! {
            // Biased so a burst of doc frames can never starve a command: the
            // user's keystroke-driven work goes out first.
            biased;

            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => return SessionEnd::Shutdown,
                // A manual reconnect tears this link down; `supervise` dials
                // again immediately.
                Some(Command::Reconnect) => return SessionEnd::ConnectionLost,
                Some(Command::WatchTranscript(target)) => {
                    if *transcript_target != target {
                        *transcript_target = target.clone();
                        // Dropping the receiver cancels the stream server-side
                        // (comet-rpc sends `{id, cancel}` on the next frame),
                        // so the engine stops serializing the old doc.
                        transcript = match target {
                            Some(chat_id) => open_transcript(client, chat_id).await,
                            None => None,
                        };
                    }
                }
                Some(Command::Call { method, params, context }) => {
                    spawn_call(client.clone(), updates.clone(), method, params, context);
                }
                Some(Command::Send { chat_id, message_id, params }) => {
                    spawn_send(client.clone(), updates.clone(), chat_id, message_id, params);
                }
                Some(Command::ListModels { harness }) => {
                    spawn_models(client.clone(), updates.clone(), harness);
                }
                Some(Command::ListRefs { repo_path, target_device }) => {
                    spawn_refs(client.clone(), updates.clone(), repo_path, target_device);
                }
                Some(Command::StartSession(start)) => {
                    spawn_start_session(client.clone(), updates.clone(), *start);
                }
            },

            frame = chats.recv() => match decode::<Vec<Chat>>(frame, "chats") {
                Frame::Value(rows) => if updates.send(Update::Chats(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = spaces.recv() => match decode::<Vec<Space>>(frame, "spaces") {
                Frame::Value(rows) => if updates.send(Update::Spaces(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = devices.recv() => match decode::<Vec<Device>>(frame, "devices") {
                Frame::Value(rows) => if updates.send(Update::Devices(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = sessions.recv() => match decode::<Vec<Session>>(frame, "sessions") {
                Frame::Value(rows) => if updates.send(Update::Sessions(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = auth.recv() => match frame {
                // Auth is the one frame with two wire shapes in flight; the
                // tolerant parser is shared with the gpui viewport.
                Some(value) => match comet_proto::view::parse_auth_state(&value) {
                    Some(state) => if updates.send(Update::Auth(Box::new(state))).is_err() {
                        return SessionEnd::AppGone;
                    },
                    None => tracing::warn!("dropping unrecognized AuthStatus frame"),
                },
                None => return SessionEnd::ConnectionLost,
            },

            // A transcript stream ending is NOT a lost connection: the engine
            // ends it when the chat is deleted. Drop it and keep going.
            frame = recv_optional(&mut transcript) => {
                let (chat_id, frame) = frame;
                match frame {
                    Some(value) => match serde_json::from_value::<Vec<SessionMessageEntry>>(value) {
                        Ok(entries) => if updates.send(Update::Transcript { chat_id, entries }).is_err() {
                            return SessionEnd::AppGone;
                        },
                        Err(err) => tracing::warn!(error = %err, "dropping malformed transcript frame"),
                    },
                    None => {
                        transcript = None;
                        *transcript_target = None;
                    }
                }
            },
        }
    }
}

/// Subscribe a chat's transcript. A failure here is not fatal to the session —
/// the chat may have just been deleted.
async fn open_transcript(
    client: &Arc<RpcClient>,
    chat_id: String,
) -> Option<(String, mpsc::UnboundedReceiver<serde_json::Value>)> {
    match client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
    {
        Ok(stream) => Some((chat_id, stream)),
        Err(err) => {
            tracing::warn!(%chat_id, error = %err, "WatchDocMessages failed");
            None
        }
    }
}

/// `recv` on the optional transcript stream, pending forever when there is
/// none, so it can sit in the `select!` unconditionally.
async fn recv_optional(
    slot: &mut Option<(String, mpsc::UnboundedReceiver<serde_json::Value>)>,
) -> (String, Option<serde_json::Value>) {
    match slot {
        Some((chat_id, stream)) => {
            let frame = stream.recv().await;
            (chat_id.clone(), frame)
        }
        None => std::future::pending().await,
    }
}

/// The three things a watch frame can mean. Kept distinct on purpose: a frame
/// we failed to parse must NOT read as a dropped connection, or one schema skew
/// between engine and viewport would put the app into a reconnect loop against
/// a perfectly healthy daemon.
enum Frame<T> {
    Value(T),
    /// Malformed — logged and ignored; the next snapshot supersedes it anyway.
    Skip,
    /// Stream closed: the engine is gone.
    Ended,
}

fn decode<T: serde::de::DeserializeOwned>(
    frame: Option<serde_json::Value>,
    what: &'static str,
) -> Frame<T> {
    let Some(value) = frame else {
        return Frame::Ended;
    };
    match serde_json::from_value(value) {
        Ok(rows) => Frame::Value(rows),
        Err(err) => {
            tracing::warn!(error = %err, what, "dropping malformed watch frame");
            Frame::Skip
        }
    }
}

fn spawn_call(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    method: &'static str,
    params: serde_json::Value,
    context: &'static str,
) {
    tokio::spawn(async move {
        if let Err(err) = client.call(method, params).await {
            let _ = updates.send(Update::Notice(format!("{context}: {err}")));
        }
    });
}

/// Fetch the model catalogue and hand it up. A failure is a notice, not a
/// silent empty list — an empty picker is indistinguishable from a broken one.
fn spawn_models(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    harness: comet_proto::HarnessId,
) {
    tokio::spawn(async move {
        match client
            .call(
                methods::LIST_MODELS,
                serde_json::json!({ "harness": harness }),
            )
            .await
        {
            Ok(value) => match serde_json::from_value::<Vec<comet_proto::Model>>(value) {
                Ok(models) => {
                    let _ = updates.send(Update::Models(models));
                }
                Err(err) => {
                    let _ = updates.send(Update::Notice(format!("Model list malformed: {err}")));
                }
            },
            Err(err) => {
                let _ = updates.send(Update::Notice(format!("Couldn't list models: {err}")));
            }
        }
    });
}

/// Fetch a space's branches for the ref picker.
fn spawn_refs(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    repo_path: String,
    target_device: Option<String>,
) {
    tokio::spawn(async move {
        let mut params = serde_json::json!({ "repoPath": repo_path });
        if let (Some(device), Some(object)) = (target_device, params.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::Value::String(device));
        }
        match client.call(methods::LIST_REFS, params).await {
            Ok(value) => match serde_json::from_value::<Vec<comet_proto::RepoRef>>(value) {
                Ok(refs) => {
                    let _ = updates.send(Update::Refs(refs));
                }
                Err(err) => {
                    let _ = updates.send(Update::Notice(format!("Branch list malformed: {err}")));
                }
            },
            // A non-git space has no refs; that is not an error worth shouting.
            Err(err) => tracing::debug!(error = %err, "ListRefs unavailable"),
        }
    });
}

/// Materialize a drafted session: worktree (if the plan calls for one), then
/// the chat row, then its first command — in that order, because each step
/// depends on the last.
fn spawn_start_session(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    start: StartSession,
) {
    use comet_proto::view::CheckoutPlan;
    tokio::spawn(async move {
        let mut cwd: Option<String> = None;
        let mut branch: Option<String> = None;

        match &start.plan {
            CheckoutPlan::CurrentCheckout { branch: name } => branch.clone_from(name),
            CheckoutPlan::ReuseWorktree { path, branch: name } => {
                cwd = Some(path.clone());
                branch = Some(name.clone());
            }
            CheckoutPlan::NewWorktree { base } => {
                branch.clone_from(base);
                if let Some(base) = base {
                    let mut params = serde_json::json!({
                        "repoPath": start.repo_path,
                        "branch": base,
                    });
                    if let (Some(device), Some(object)) =
                        (start.target_device.clone(), params.as_object_mut())
                    {
                        object.insert("targetDeviceId".into(), serde_json::Value::String(device));
                    }
                    match client.call(methods::CREATE_WORKTREE, params).await {
                        Ok(value) => match serde_json::from_value::<comet_proto::Worktree>(value) {
                            Ok(worktree) => cwd = Some(worktree.path),
                            Err(err) => {
                                let _ = updates.send(Update::Notice(format!(
                                    "Worktree reply malformed: {err}"
                                )));
                                return;
                            }
                        },
                        Err(err) => {
                            let _ = updates.send(Update::Notice(format!("Worktree failed: {err}")));
                            return;
                        }
                    }
                }
            }
        }

        let mut mutate = serde_json::json!({
            "op": "createChat",
            "chatId": start.chat_id,
            "spaceId": start.space_id,
        });
        if let Some(object) = mutate.as_object_mut() {
            if let Some(cwd) = &cwd {
                object.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
            }
            if let Some(branch) = &branch {
                object.insert("branch".into(), serde_json::Value::String(branch.clone()));
            }
            if let Some(config) = start.config {
                object.insert("config".into(), config);
            }
        }
        if let Err(err) = client.call(methods::MUTATE, mutate).await {
            let _ = updates.send(Update::Notice(format!(
                "Couldn't create the session: {err}"
            )));
            return;
        }

        // The run's cwd must match what the chat was created with.
        let mut command = start.command;
        if let (Some(cwd), Some(request)) = (
            &cwd,
            command.get_mut("request").and_then(|r| r.as_object_mut()),
        ) {
            request.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
        }
        let params = serde_json::json!({ "chatId": start.chat_id, "command": command });
        match client.call(methods::QUEUE_COMMAND, params).await {
            Ok(_) => {
                let _ = updates.send(Update::SessionStarted {
                    chat_id: start.chat_id,
                });
            }
            Err(err) => {
                let _ = updates.send(Update::SendFailed {
                    chat_id: start.chat_id,
                    message_id: start.message_id,
                    error: err.to_string(),
                });
            }
        }
    });
}

fn spawn_send(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    chat_id: String,
    message_id: String,
    params: serde_json::Value,
) {
    tokio::spawn(async move {
        if let Err(err) = client.call(methods::QUEUE_COMMAND, params).await {
            let _ = updates.send(Update::SendFailed {
                chat_id,
                message_id,
                error: err.to_string(),
            });
        }
    });
}
