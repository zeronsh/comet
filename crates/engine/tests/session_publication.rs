//! Real Codex adapter + engine persistence + ChatClient over a loopback relay.
//! Faults are injected only in temporary profiles and the test relay.
use futures::{SinkExt, StreamExt, future::BoxFuture};
use loro::{ExportMode, LoroDoc};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use zeron_doc::{MessageRole, MessageStatus, SessionDoc};
use zeron_engine::{
    EdgeConfig, EngineCore, EngineProfile, HarnessRegistry, chat2_host::EngineChatSink,
};
use zeron_harness::CodexHarness;
use zeron_proto::{HarnessId, ReasoningLevel, RunRequest, SandboxLevel, SessionStatus};
use zeron_sync::chat_frames::{decode, encode, frame_type};
use zeron_sync::{
    ChatClient, ChatDocSink, CheckpointFetcher, DocsStore, SyncError, chat_client::RowImportOutcome,
};
const CHAT: &str = "publication-regression";

struct Fetch(Vec<u8>);
impl CheckpointFetcher for Fetch {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let bytes = self.0.clone();
        Box::pin(async move { Ok(bytes) })
    }
}
#[derive(Default)]
struct Room {
    rows: Vec<(String, Vec<u8>)>,
    ids: HashMap<String, u64>,
    clients: Vec<mpsc::UnboundedSender<Vec<u8>>>,
    checkpoint_attempts: usize,
    checkpoints: Vec<(Vec<u8>, String)>,
}
async fn relay(
    checkpoint: Vec<u8>,
) -> (
    String,
    Arc<Mutex<Room>>,
    Arc<AtomicBool>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Room::default()));
    let ack = Arc::new(AtomicBool::new(false));
    let shared = state.clone();
    let send_ack = ack.clone();
    let frontier = LoroDoc::decode_import_blob_meta(&checkpoint, true)
        .unwrap()
        .partial_end_vv
        .encode();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let state = shared.clone();
            let ack = send_ack.clone();
            let frontier = frontier.clone();
            let size = checkpoint.len();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut prefix = [0; 5];
                if stream.peek(&mut prefix).await.unwrap_or(0) == 5 && &prefix == b"POST " {
                    let mut data = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0; 8192];
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..n]);
                        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    while data.len() < header_end + length {
                        let mut chunk = [0; 8192];
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..n]);
                    }
                    let header_text = String::from_utf8_lossy(&data[..header_end]);
                    let frontier = header_text
                        .lines()
                        .find_map(|l| l.strip_prefix("x-chat2-frontier: "))
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let accepted = {
                        let mut st = state.lock().unwrap();
                        st.checkpoint_attempts += 1;
                        if st.checkpoint_attempts > 1 {
                            st.checkpoints
                                .push((data[header_end..header_end + length].to_vec(), frontier));
                            true
                        } else {
                            false
                        }
                    };
                    let reply = if accepted {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .as_slice()
                    } else {
                        b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                    };
                    let _ = stream.write_all(reply).await;
                    return;
                }
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let (mut tx, mut rx) = ws.split();
                let (out, mut incoming) = mpsc::unbounded_channel();
                state.lock().unwrap().clients.push(out.clone());
                let writer = tokio::spawn(async move {
                    while let Some(bytes) = incoming.recv().await {
                        if tx.send(bytes.into()).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(message)) = rx.next().await {
                    let Some(frame) = decode(&message.into_data()) else {
                        continue;
                    };
                    let mut st = state.lock().unwrap();
                    let head = 42 + st.rows.len() as u64;
                    match frame.kind {
                        frame_type::HELLO => {
                            let _ = out.send(encode(frame_type::STATE,&serde_json::json!({"headSeq":head,"seqFloor":42,"checkpointSeq":42,"checkpointSize":size,"rowCount":st.rows.len(),"rowBytes":0}),&frontier));
                        }
                        frame_type::ROWS_REQ => {
                            let after = frame.header["after"].as_u64().unwrap();
                            for (i, (id, bytes)) in st.rows.iter().enumerate() {
                                let seq = 43 + i as u64;
                                if seq > after {
                                    let _ = out.send(encode(frame_type::ROW,&serde_json::json!({"seq":seq,"device":"writer","batchId":id}),bytes));
                                }
                            }
                            let _ = out.send(encode(
                                frame_type::ROWS_DONE,
                                &serde_json::json!({"headSeq":head}),
                                &[],
                            ));
                        }
                        frame_type::PUSH => {
                            let id = frame.header["batchId"].as_str().unwrap().to_string();
                            let seq = if let Some(seq) = st.ids.get(&id) {
                                *seq
                            } else {
                                let seq = 43 + st.rows.len() as u64;
                                st.ids.insert(id.clone(), seq);
                                st.rows.push((id.clone(), frame.payload.clone()));
                                let row = encode(
                                    frame_type::ROW,
                                    &serde_json::json!({"seq":seq,"device":"writer","batchId":id}),
                                    &frame.payload,
                                );
                                st.clients.retain(|c| c.send(row.clone()).is_ok());
                                seq
                            };
                            if ack.load(Ordering::SeqCst) {
                                let _ = out.send(encode(
                                    frame_type::ACK,
                                    &serde_json::json!({"batchId":id,"seq":seq}),
                                    &[],
                                ));
                            }
                        }
                        frame_type::PROBE => {
                            let _ = out.send(encode(
                                frame_type::PROBE_OK,
                                &serde_json::json!({"headSeq":head}),
                                &[],
                            ));
                        }
                        _ => {}
                    }
                }
                writer.abort();
            });
        }
    });
    (url, state, ack, task)
}
async fn wait(mut f: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(120), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition converges");
}
fn assemble(profile: &EngineProfile, live: bool) -> EngineCore {
    let harness = if live {
        CodexHarness::new().with_executable(
            std::env::var_os("SESSION_SYNC_CODEX")
                .expect("set SESSION_SYNC_CODEX to installed codex"),
        )
    } else {
        CodexHarness::new().with_executable(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../harness/tests/fixtures/fake-codex.sh"),
        )
    };
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(harness));
    // The closed port guarantees no bytes escape to a production relay.
    EngineCore::assemble_with_profile(
        profile.clone(),
        registry,
        HarnessId::Codex,
        Some(EdgeConfig::with_static_token("http://127.0.0.1:1", "test")),
    )
    .unwrap()
}
async fn turn(core: &EngineCore, cwd: &std::path::Path, live: bool, second: bool) {
    let prompt = if live {
        if second {
            "Continue. Reply exactly PUBLICATION-SECOND. Do not use tools or modify files."
        } else {
            "Reply exactly PUBLICATION-FIRST. Do not use tools or modify files."
        }
    } else if second {
        "scenario:publication"
    } else {
        "scenario:publication"
    };
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Codex,
            RunRequest {
                prompt: prompt.into(),
                harness: None,
                model: live
                    .then(|| std::env::var("SESSION_SYNC_MODEL").unwrap_or("gpt-6-astra".into())),
                reasoning: Some(ReasoningLevel::Medium),
                model_options: Default::default(),
                cwd: cwd.display().to_string(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: true,
                attachments: vec![],
                worktree: None,
                resume: None,
            },
            Some(if second { "second-user" } else { "first-user" }.into()),
        )
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.status == SessionStatus::Idle)
            && core
                .doc_host
                .open(CHAT)
                .unwrap()
                .doc()
                .read_entries()
                .unwrap()
                .iter()
                .filter(|e| {
                    e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
                })
                .count()
                >= if second { 2 } else { 1 }
    })
    .await;
}
async fn regression(live: bool) {
    let temp = tempfile::tempdir().unwrap();
    let profile = EngineProfile::development(temp.path(), "test-org", "test-user");
    let core = assemble(&profile, live);
    turn(&core, temp.path(), live, false).await;
    println!("first turn complete");
    core.shutdown().await;
    drop(core);
    let store = Arc::new(DocsStore::open(profile.store_root()).unwrap());
    let old = store.load_snapshot(CHAT).unwrap().unwrap();
    let old_doc = LoroDoc::new();
    old_doc.import(&old).unwrap();
    // The incident's predecessor shape: cleanup after a completed assistant.
    for i in 0..136 {
        old_doc
            .get_map("cleanup")
            .insert(&format!("subagent-{i}"), "failed")
            .unwrap();
        old_doc.commit();
    }
    let orphan_snapshot = old_doc.export(ExportMode::Snapshot).unwrap();
    let before_continue = old_doc.oplog_vv();
    // Emulate a pre-upgrade snapshot whose memory-only outbox was lost.
    store.delete_snapshot(CHAT).unwrap();
    store
        .save_snapshot_with_cursor(CHAT, &orphan_snapshot, 42, 2)
        .unwrap();
    let core = assemble(&profile, live);
    turn(&core, temp.path(), live, true).await;
    println!("resumed turn complete");
    let handle = core.doc_host.open(CHAT).unwrap();
    if live {
        let (events, _) = core.sessions.subscribe(CHAT, 0).unwrap();
        let native_sessions: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                zeron_proto::AgentEvent::SessionStarted { session_id, .. } => Some(session_id),
                _ => None,
            })
            .collect();
        assert!(native_sessions.len() >= 2);
        assert!(
            native_sessions.iter().all(|id| *id == native_sessions[0]),
            "real Continue resumes the same native session"
        );
    }
    let expected = handle.doc().read_entries().unwrap();
    let new_rows = handle
        .doc()
        .doc()
        .export(ExportMode::updates(&before_continue))
        .unwrap();
    let desktop = Arc::new(SessionDoc::from_doc(LoroDoc::new()));
    desktop.doc().import(&old).unwrap();
    let desktop_dir = tempfile::tempdir().unwrap();
    let desktop_store = Arc::new(DocsStore::open(desktop_dir.path()).unwrap());
    let desktop_sink = Arc::new(EngineChatSink::new(&desktop, desktop_store, CHAT));
    assert_eq!(
        desktop_sink.apply_row(&new_rows, 43),
        RowImportOutcome::PendingDependencies
    );
    assert!(
        !desktop
            .read_entries()
            .unwrap()
            .iter()
            .any(|e| e.id == "second-user"),
        "old publication behavior hides the successful resumed turn"
    );
    println!("reproduced: successful resumed Codex turn is invisible without cleanup predecessors");
    drop(handle);
    core.shutdown().await;
    drop(core);
    let pending = store.pending_chat_updates(CHAT).unwrap();
    assert!(!pending.is_empty());
    let host = Arc::new(SessionDoc::from_doc(LoroDoc::new()));
    host.doc()
        .import(&store.load_snapshot(CHAT).unwrap().unwrap())
        .unwrap();
    for (_, bytes) in &pending {
        host.doc().import(bytes).unwrap();
    }
    let sink = Arc::new(EngineChatSink::new(&host, store.clone(), CHAT));
    let (url, room, acks, server) = relay(old.clone()).await;
    let viewer = ChatClient::connect(
        &url,
        desktop_sink,
        Arc::new(Fetch(old.clone())),
        "desktop",
        42,
    )
    .await
    .unwrap();
    let writer = ChatClient::connect(
        &url,
        sink.clone(),
        Arc::new(Fetch(old.clone())),
        "writer",
        42,
    )
    .await
    .unwrap();
    wait(|| room.lock().unwrap().rows.len() == pending.len()).await;
    wait(|| desktop.read_entries().unwrap().len() == expected.len()).await;
    assert_eq!(
        store.pending_chat_updates(CHAT).unwrap().len(),
        pending.len(),
        "lost ACK must retain durable batches"
    );
    drop(writer);
    acks.store(true, Ordering::SeqCst);
    let writer = ChatClient::connect(&url, sink, Arc::new(Fetch(old)), "writer", 42)
        .await
        .unwrap();
    wait(|| store.pending_chat_updates(CHAT).unwrap().is_empty()).await;
    assert_eq!(
        room.lock().unwrap().rows.len(),
        pending.len(),
        "recreated actor reuses batch IDs"
    );
    let actual = desktop.read_entries().unwrap();
    assert_eq!(
        actual, expected,
        "peer transcript, statuses and parts must all match"
    );
    println!(
        "fixed: durable replay restores both turns; lost-ACK restart adds no duplicate relay rows"
    );
    writer.shutdown().await;
    viewer.shutdown().await;
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_adapter_restart_publication() {
    regression(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires installed authenticated Codex; uses only isolated test sessions"]
async fn real_codex_restart_publication() {
    regression(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnected_cleanup_is_durable_before_snapshot_debounce() {
    use zeron_engine::{DocHost, DocHostConfig};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let config = || DocHostConfig {
        device_id: "writer".into(),
        default_harness: HarnessId::Codex,
        edge: Some(EdgeConfig::with_static_token("http://127.0.0.1:1", "test")),
    };
    let host = DocHost::new(store.clone(), config());
    let handle = host.open(CHAT).unwrap();
    let before = handle.doc().export_snapshot().unwrap();
    host.disconnect_edge();
    for i in 0..136 {
        handle
            .doc()
            .doc()
            .get_map("cleanup")
            .insert(&format!("chip-{i}"), "failed")
            .unwrap();
        handle.doc().doc().commit();
    }
    let expected = handle.doc().doc().oplog_vv();
    assert!(store.pending_chat_updates(CHAT).unwrap().len() >= 136);
    drop(handle);
    host.shutdown_workers().await;
    drop(host);
    // Crash shape: the durable update journal is newer than the last snapshot.
    store
        .save_snapshot_with_cursor(CHAT, &before, 42, 2)
        .unwrap();
    let reopened = DocHost::new(store.clone(), config());
    let handle = reopened.open(CHAT).unwrap();
    assert!(handle.doc().doc().oplog_vv().includes_vv(&expected));
    assert_eq!(handle.doc().doc().get_map("cleanup").len(), 136);
    drop(handle);
    reopened.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires private incident snapshot paths; all writes remain in a temporary store"]
async fn supplied_incident_snapshots_reconcile_via_durable_bootstrap() {
    use zeron_engine::{DocHost, DocHostConfig};
    let remote = std::fs::read(std::env::var("SESSION_SYNC_REMOTE_SNAPSHOT").unwrap()).unwrap();
    let before = std::fs::read(std::env::var("SESSION_SYNC_DESKTOP_SNAPSHOT").unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    store
        .save_snapshot_with_cursor(CHAT, &remote, 40331, 2)
        .unwrap();
    let host = DocHost::new(
        store.clone(),
        DocHostConfig {
            device_id: "test".into(),
            default_harness: HarnessId::Codex,
            edge: Some(EdgeConfig::with_static_token("http://127.0.0.1:1", "test")),
        },
    );
    let handle = host.open(CHAT).unwrap();
    let expected = handle.doc().read_entries().unwrap();
    let desktop = LoroDoc::new();
    desktop.import(&before).unwrap();
    let desktop = Arc::new(SessionDoc::from_doc(desktop));
    let updates = store.pending_chat_updates(CHAT).unwrap();
    assert!(!updates.is_empty());
    for (_, bytes) in &updates {
        assert!(bytes.len() <= zeron_sync::chat_client::MAX_PUSH_BYTES);
    }
    let (url, _, acks, server) = relay(before.clone()).await;
    acks.store(true, Ordering::SeqCst);
    let viewer_dir = tempfile::tempdir().unwrap();
    let viewer_sink = Arc::new(EngineChatSink::new(
        &desktop,
        Arc::new(DocsStore::open(viewer_dir.path()).unwrap()),
        CHAT,
    ));
    let viewer = ChatClient::connect(
        &url,
        viewer_sink,
        Arc::new(Fetch(before.clone())),
        "viewer",
        42,
    )
    .await
    .unwrap();
    let source = handle.doc_arc();
    let writer_sink = Arc::new(EngineChatSink::new(&source, store.clone(), CHAT));
    let writer = ChatClient::connect(&url, writer_sink, Arc::new(Fetch(before)), "writer", 42)
        .await
        .unwrap();
    wait(|| store.pending_chat_updates(CHAT).unwrap().is_empty()).await;
    wait(|| {
        desktop
            .doc()
            .oplog_vv()
            .includes_vv(&source.doc().oplog_vv())
    })
    .await;
    assert!(
        desktop.read_entries().unwrap() == expected,
        "incident transcript differs"
    );
    assert!(
        desktop.read_commands().unwrap() == handle.doc().read_commands().unwrap(),
        "incident commands differ"
    );
    assert!(
        desktop
            .doc()
            .oplog_vv()
            .includes_vv(&handle.doc().doc().oplog_vv())
    );
    println!(
        "incident bootstrap: {} bounded rows, {} bytes, {} messages; exact transcript and commands restored",
        updates.len(),
        updates.iter().map(|(_, b)| b.len()).sum::<usize>(),
        expected.len()
    );
    writer.shutdown().await;
    viewer.shutdown().await;
    server.abort();
    drop(handle);
    host.shutdown_workers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_update_retries_failed_checkpoint_and_retires_only_after_success() {
    use base64::Engine as _;
    use zeron_engine::{DocHost, DocHostConfig};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let initial = SessionDoc::init(CHAT).unwrap().export_snapshot().unwrap();
    store
        .save_snapshot_with_cursor(CHAT, &initial, 42, 2)
        .unwrap();
    store.initialize_chat_outbox(CHAT, &[]).unwrap();
    let (url, room, acks, server) = relay(initial).await;
    acks.store(true, Ordering::SeqCst);
    let host = DocHost::new(
        store.clone(),
        DocHostConfig {
            device_id: "writer".into(),
            default_harness: HarnessId::Codex,
            edge: Some(EdgeConfig::with_static_token(
                url.replacen("ws", "http", 1),
                "test",
            )),
        },
    );
    let handle = host.open(CHAT).unwrap();
    // Incompressible single map value exceeds the relay row cap. Its operation
    // cannot be split; checkpoint retry is the durable fallback.
    let mut state = 0x123456789abcdefu64;
    let text: String = (0..2_000_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (33 + (state % 90) as u8) as char
        })
        .collect();
    handle
        .doc()
        .doc()
        .get_map("large")
        .insert("value", text)
        .unwrap();
    handle.doc().doc().commit();
    wait(|| room.lock().unwrap().checkpoint_attempts >= 1).await;
    assert!(
        !store.rejected_chat_updates(CHAT).unwrap().is_empty(),
        "failed POST must retain obligation"
    );
    wait(|| !room.lock().unwrap().checkpoints.is_empty()).await;
    wait(|| store.rejected_chat_updates(CHAT).unwrap().is_empty()).await;
    let (snapshot, frontier) = room.lock().unwrap().checkpoints.last().unwrap().clone();
    let published = LoroDoc::new();
    assert!(published.import(&snapshot).unwrap().pending.is_none());
    assert_eq!(
        published.get_map("large").get_deep_value(),
        handle.doc().doc().get_map("large").get_deep_value()
    );
    let vv = loro::VersionVector::decode(
        &base64::engine::general_purpose::STANDARD
            .decode(frontier)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        vv,
        published.oplog_vv(),
        "frontier must describe the captured bytes"
    );
    drop(handle);
    host.shutdown_workers().await;
    server.abort();
}
