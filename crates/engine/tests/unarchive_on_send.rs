//! Sending a message to an archived chat revives it: `queue_command` flips the
//! workspace row's `archived` flag back off for message-bearing commands (Run,
//! Steer) — and only those; an Interrupt leaves the archive state alone.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{MessageRole, MessageStatus, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};

const CHAT: &str = "chat-unarchive";

/// Completes a one-line turn for any request.
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
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-ua".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: format!("ack: {}", request.prompt),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-ua".into()),
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

fn complete_assistant_count(core: &EngineCore) -> usize {
    let entries: Vec<SessionMessageEntry> = core
        .doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn run_payload(message_id: &str) -> SessionCommandPayload {
    SessionCommandPayload::Run {
        request: RunRequest {
            prompt: "back from the archive".into(),
            harness: None,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "~".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            worktree: None,
            resume: None,
        },
        message_id: message_id.into(),
    }
}

fn archived(core: &EngineCore) -> bool {
    core.workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists")
        .archived
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_a_message_unarchives_the_chat() {
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
            serde_json::json!({
                "op": "createChat",
                "chatId": CHAT,
                "deviceId": core.device_id,
            }),
        )
        .await
        .expect("createChat");
    // Pre-title so the auto-titler's harness request stays out of the flow.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    core.workspace
        .set_chat_archived(CHAT, true)
        .expect("archive chat");
    assert!(archived(&core), "precondition: chat is archived");

    // A Run command (the composer's send) revives the row synchronously.
    core.doc_host
        .queue_command(CHAT, run_payload("msg-ua-1"))
        .expect("queue run command");
    assert!(
        !archived(&core),
        "sending a message must unarchive the chat"
    );
    wait_for(|| complete_assistant_count(&core) == 1, "turn to complete").await;

    // A non-message command must NOT revive it.
    core.workspace
        .set_chat_archived(CHAT, true)
        .expect("re-archive chat");
    core.doc_host
        .queue_command(CHAT, SessionCommandPayload::Interrupt {})
        .expect("queue interrupt");
    assert!(
        archived(&core),
        "an interrupt is not a message and must not unarchive"
    );

    core.shutdown().await;
}
