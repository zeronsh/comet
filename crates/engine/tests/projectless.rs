//! "Don't work in a project" against a real engine: a chat minted through the
//! UI's exact wire shape (`Mutate createChat` with a `deviceId` and no
//! `spaceId`) stores cwd `~`, spawns its run from the host's REAL home dir,
//! and never mints a space row — the two failure modes of pre-#40 engines
//! (a phantom project at root, and the run dying on the literal `~`).

use std::sync::{Arc, Mutex};
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

const CHAT: &str = "chat-projectless";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

/// Records every `RunRequest` it receives (the cwd probe), then completes a
/// one-line turn.
struct RecordingHarness {
    requests: RequestLog,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recording"
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
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        let events: Vec<Result<AgentEvent, HarnessError>> = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd.clone(),
                session_id: "sess-np".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: format!("ack: {}", request.prompt),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-np".into()),
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

#[tokio::test(flavor = "multi_thread")]
async fn projectless_chat_runs_from_home_and_mints_no_space() {
    let tmp = tempfile::tempdir().unwrap();
    let requests: RequestLog = RequestLog::default();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness {
        requests: requests.clone(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");

    // The composer's exact wire shape for "Don't work in a project": a
    // deviceId, no spaceId, no cwd.
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
        .expect("createChat without a space");
    // Pre-title so the auto-titler's own harness request stays out of the log.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    assert_eq!(chat.space_id, None, "project-less chat must carry no space");
    assert_eq!(chat.cwd.as_deref(), Some("~"), "cwd defaults to `~`");
    assert_eq!(chat.device_id, core.device_id);

    // Run exactly as the composer sends it: the chat's stored cwd, `~`.
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: "hello from no project".into(),
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
                message_id: "msg-np-1".into(),
            },
        )
        .expect("queue run command");
    wait_for(|| complete_assistant_count(&core) == 1, "turn to complete").await;

    // The harness must see the host's real home dir, not the literal `~`.
    let cwds: Vec<String> = requests
        .lock()
        .expect("request log")
        .iter()
        .map(|r| r.cwd.clone())
        .collect();
    let home = std::env::var("HOME").expect("HOME set in test env");
    assert_eq!(cwds, vec![home], "run spawns from the expanded home dir");

    // And no phantom project: the flow must not mint any space row.
    let spaces = core.workspace.read_spaces().expect("read spaces");
    assert!(
        spaces.is_empty(),
        "project-less chat minted a space: {spaces:?}"
    );

    core.shutdown().await;
}
