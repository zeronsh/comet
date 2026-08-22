//! Queued attachments (send-is-a-local-write): a Run command carrying
//! `pending://` refs defers — Pending, unprocessed — until the bytes land in
//! this device's uploads dir, then executes with the refs rewritten to
//! absolute paths (2026-08-19 incident: attachment staging used to be a
//! blocking pre-step in FRONT of QueueCommand, so a dead peer link killed the
//! send instead of queueing it).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{
    MessageRole, MessageStatus, SessionCommandPayload, SessionCommandStatus, SessionMessageEntry,
};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};

const CHAT: &str = "chat-queued-att";

struct AckHarness;

#[async_trait]
impl Harness for AckHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Ack"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        // The harness must see ordinary local files, never pending refs.
        assert!(
            !request.prompt.contains("pending://"),
            "prompt reached the harness with unresolved pending refs: {}",
            request.prompt
        );
        for path in &request.attachments {
            assert!(
                std::path::Path::new(path).is_file(),
                "attachment path not a real file: {path}"
            );
        }
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-qa".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta { text: "ack".into() }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-qa".into()),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    entries(core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn run_payload(message_id: &str, pending_ref: &str) -> SessionCommandPayload {
    SessionCommandPayload::Run {
        request: RunRequest {
            prompt: format!(
                "look at this\n\nAttached images (local files — open them to view):\n- {pending_ref}"
            ),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "~".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: vec![pending_ref.to_string()],
            worktree: None,
            resume: None,
        },
        message_id: message_id.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn run_defers_until_attachment_bytes_land_then_executes_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(AckHarness));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");

    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            zeron_rpc::methods::MUTATE,
            serde_json::json!({ "op": "createChat", "chatId": CHAT, "deviceId": core.device_id }),
        )
        .await
        .expect("createChat");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    // Queue a Run whose attachment bytes have NOT landed yet: it must sit
    // Pending — deferred, unprocessed, no turn — instead of running without
    // its image (or worse, handing the harness a dangling ref).
    let pending_ref = "pending://att-1/photo one.png";
    core.doc_host
        .queue_command(CHAT, run_payload("msg-qa-1", pending_ref))
        .expect("queue run command");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        complete_assistant_count(&core),
        0,
        "run must defer while attachment bytes are in transit"
    );
    let commands = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_commands()
        .unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].status,
        SessionCommandStatus::Pending,
        "deferred command must stay Pending (not Rejected/Expired)"
    );

    // The bytes land (the engine-to-engine transfer ends in exactly these two
    // RPCs on the host): UploadChunk + UploadCommit under the SAME identity.
    // The commit handler kicks the drains — no timers involved.
    client
        .call(
            zeron_rpc::methods::UPLOAD_CHUNK,
            serde_json::json!({
                "uploadId": "att-1", "seq": 0, "data": BASE64.encode(b"png-bytes"),
            }),
        )
        .await
        .expect("upload chunk");
    client
        .call(
            zeron_rpc::methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "att-1", "fileName": "photo one.png" }),
        )
        .await
        .expect("upload commit");

    wait_for(
        || complete_assistant_count(&core) == 1,
        "deferred run to execute",
    )
    .await;

    // The persisted user entry carries the rewritten ABSOLUTE path (legacy
    // transcript shape — old clients render it as always), not the ref.
    let user_text = entries(&core)
        .iter()
        .find(|e| e.role == MessageRole::User)
        .and_then(|e| {
            e.parts.iter().find_map(|p| match p {
                zeron_doc::MessagePart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
        .expect("user entry persisted");
    assert!(
        !user_text.contains("pending://"),
        "persisted text must not leak pending refs: {user_text}"
    );
    assert!(
        user_text.contains("att-1-photo_one.png"),
        "persisted text names the committed file: {user_text}"
    );

    core.shutdown().await;
}
