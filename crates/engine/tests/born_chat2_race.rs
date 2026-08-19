//! The born-gen2 race (2026-08-11): the composer attaches the transcript
//! watch — opening the chat doc — BEFORE its own `Mutate createChat` lands,
//! so `open()` sees no registry row. Defaulting the absent row to gen 1
//! minted a brand-new legacy s2 room post-cutover: the host ran the whole
//! session against a room no other device reads (they follow the row's
//! roomGen 2 to an empty chat2 room), the run's live doc ref blocked the
//! cutover watcher's drop-and-reopen heal, the stuck handle got retired
//! (suppressing every snapshot save), and the transcript vanished on the
//! next restart. An absent row must mean "being born on chat2".

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

const CHAT: &str = "chat-born-gen2-race";

/// Completes a one-line turn (the transcript payload the test asserts on).
struct OneLinerHarness;

#[async_trait]
impl Harness for OneLinerHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "OneLiner"
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
                session_id: "sess-race".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "the codeword is PINEAPPLE".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-race".into()),
            }),
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn assemble(dir: &std::path::Path) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(OneLinerHarness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
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

#[tokio::test(flavor = "multi_thread")]
async fn transcript_survives_open_racing_create_chat() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");

    let watch_holder;
    {
        let core = assemble(&dir);

        // The racing transcript watch: the doc opens BEFORE any registry row
        // exists, and a live doc ref (the run, in production) pins the handle
        // across the row's arrival so no drop-and-reopen heal can save us —
        // the open itself must land on chat2.
        let handle = core.doc_host.open(CHAT).expect("open before createChat");
        watch_holder = handle.watch_messages();
        let live_writer_ref = handle.doc_arc();

        // The mint lands a beat later, exactly as the composer sends it.
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
            .expect("createChat lands after the open");
        // Pre-title so the auto-titler's harness request stays out of the run.
        core.workspace
            .rename_chat(CHAT, "Pre-titled")
            .expect("rename chat");

        core.doc_host
            .queue_command(
                CHAT,
                SessionCommandPayload::Run {
                    request: RunRequest {
                        prompt: "what's the codeword?".into(),
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
                    message_id: "msg-race-1".into(),
                },
            )
            .expect("queue run command");
        wait_for(|| complete_assistant_count(&core) == 1, "turn to complete").await;

        // The row arriving mid-life must not blank the transcript (the old
        // watcher retired the handle; a reopen then built a fresh empty doc).
        let mid = entries(&core);
        assert!(
            mid.iter().any(|e| e.role == MessageRole::User),
            "user message lost after registry row arrived: {mid:?}"
        );

        drop(live_writer_ref);
        core.shutdown().await;
    }
    drop(watch_holder);

    // Restart on the same data dir: the transcript must have persisted. The
    // stuck s2 handle's `retired` flag used to suppress every snapshot save,
    // so the doc's only copy died with the process.
    let core = assemble(&dir);
    let after = entries(&core);
    assert_eq!(
        complete_assistant_count(&core),
        1,
        "assistant turn lost across restart: {after:?}"
    );
    assert!(
        after.iter().any(|e| e.role == MessageRole::User),
        "user message lost across restart: {after:?}"
    );
    core.shutdown().await;
}
