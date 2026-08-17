//! The pending-message queue: what happens to a message typed while the agent
//! is busy, and how it eventually reaches the agent.
//!
//! The policy under test (`DocHost::drain_queue`):
//! - idle agent → the queue drains immediately, in order;
//! - busy agent that only takes input at a turn boundary → the queue HOLDS,
//!   and flushes when the turn ends;
//! - busy agent that takes input mid-turn → steered in;
//! - "send now" → interrupts whatever is running.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use zeron_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputQuestion,
};

const CHAT: &str = "chat-queue";

/// A turn that does not end until the test says so, so "the agent is busy" is
/// a state the test controls rather than races.
struct HeldHarness {
    steering: SteeringMode,
    finish: tokio::sync::broadcast::Sender<()>,
    prompts: Arc<Mutex<Vec<String>>>,
    /// Park the first turn on a question instead of just hanging, so the chat
    /// sits in `AwaitingInput` rather than `Working`.
    asks: bool,
}

impl HeldHarness {
    fn new(steering: SteeringMode) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        Self::build(steering, false)
    }

    fn asking(steering: SteeringMode) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        Self::build(steering, true)
    }

    fn build(steering: SteeringMode, asks: bool) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let (finish, _) = tokio::sync::broadcast::channel(16);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                steering,
                finish,
                prompts: prompts.clone(),
                asks,
            }),
            prompts,
        )
    }
}

#[async_trait]
impl Harness for HeldHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Held"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        self.steering
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
        self.prompts.lock().unwrap().push(request.prompt.clone());
        if self.asks {
            // Only the engine can mint a request id it will honour, so the
            // question has to go through controls rather than the stream.
            let _answer = (_controls.request_input)(vec![UserInputQuestion {
                id: "q1".into(),
                header: "Choose".into(),
                question: "which one?".into(),
                options: vec!["a".into(), "b".into()],
                multi_select: false,
            }]);
        }
        let mut finish = self.finish.subscribe();
        let started = futures::stream::iter(vec![Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: request.cwd.clone(),
            session_id: "sess-queue".into(),
            assistant_message_id: format!("a-{}", request.prompt),
        })]);
        let done = futures::stream::once(async move {
            let _ = finish.recv().await;
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("sess-queue".into()),
            })
        });
        Ok(started.chain(done).boxed())
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

fn queue_texts(core: &EngineCore) -> Vec<String> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_queue().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|item| item.text)
        .collect()
}

fn user_messages(core: &EngineCore) -> Vec<String> {
    let entries: Vec<SessionMessageEntry> = core
        .doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .map(|e| {
            e.parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

async fn setup(steering: SteeringMode) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    setup_with(HeldHarness::new(steering)).await
}

/// [`setup`] with a harness whose turn parks on a question.
async fn setup_asking(
    steering: SteeringMode,
) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    setup_with(HeldHarness::asking(steering)).await
}

async fn setup_with(
    built: (Arc<HeldHarness>, Arc<Mutex<Vec<String>>>),
) -> (EngineCore, Arc<HeldHarness>, Arc<Mutex<Vec<String>>>) {
    let tmp = tempfile::tempdir().unwrap();
    // Leak the tempdir guard: the engine outlives this helper and the test only
    // cares that the path is unique per run.
    let path = tmp.keep();
    let (harness, prompts) = built;
    let registry = HarnessRegistry::new();
    registry.register(harness.clone());
    let core = EngineCore::assemble(
        &path.join("data"),
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
    // Pre-title so the auto-titler never dispatches a harness request of its own.
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");
    (core, harness, prompts)
}

/// Nothing is running, so a queued message is just a message: it goes out at
/// once, and the queue is empty again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_drains_immediately_when_the_agent_is_idle() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "first", Vec::new())
        .expect("queue message");

    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "first"),
        "the queued message to dispatch",
    )
    .await;
    wait_for(|| queue_texts(&core).is_empty(), "the queue to empty").await;

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A turn-boundary agent cannot take a message mid-turn, so the queue holds it —
/// visible, editable — and sends it when the turn ends. This is the case the
/// composer's steering warning is about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_holds_during_a_turn_and_flushes_in_order_at_its_end() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    // Start a turn and let it hang.
    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "second", Vec::new())
        .expect("queue second");
    core.doc_host
        .queue_message(CHAT, "third", Vec::new())
        .expect("queue third");

    // Both must still be sitting there: the agent is busy and cannot be steered.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(queue_texts(&core), vec!["second", "third"]);
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "second"));

    // Turn ends → the queue flushes, head first.
    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "second"),
        "the queue to flush at turn end",
    )
    .await;
    assert_eq!(
        queue_texts(&core),
        vec!["third"],
        "only the head goes: the next turn is now running"
    );

    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "third"),
        "the rest of the queue to flush",
    )
    .await;
    let order = prompts.lock().unwrap().clone();
    let sent: Vec<&String> = order
        .iter()
        .filter(|p| ["opening", "second", "third"].contains(&p.as_str()))
        .collect();
    assert_eq!(
        sent,
        vec!["opening", "second", "third"],
        "queued messages keep their order"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// A `Steer` command asks for the running turn directly — a client that decided
/// for itself, and the path a question's follow-up prompt takes. It obeys the
/// same rule as a typed message: a turn-boundary agent's mailbox is not read
/// mid-turn, so the prompt is held rather than posted into it.
///
/// Posting it anyway is the 2026-08-13 report: on `cursor-agent` the follow-up
/// went into the mailbox, the turn ended interrupted, and the message sat in the
/// transcript looking sent with the agent never seeing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_command_holds_for_an_agent_that_takes_no_mid_turn_prompt() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the turn to start",
    )
    .await;

    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "and also this".into(),
                message_id: Some("m-steer".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || queue_texts(&core) == vec!["and also this"],
        "the steer to be held in the queue",
    )
    .await;
    // Held means held: not shown as sent, and not with the agent.
    assert!(!user_messages(&core).iter().any(|m| m == "and also this"));
    assert!(!prompts.lock().unwrap().iter().any(|p| p == "and also this"));

    // And it goes on its own when the turn ends — nobody re-sends it.
    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "and also this"),
        "the held steer to flush at turn end",
    )
    .await;
    assert!(queue_texts(&core).is_empty());

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// An agent that takes mid-turn input: the message goes straight into the
/// running turn, and the queue stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_sends_into_a_live_turn_when_the_agent_takes_mid_turn_input() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "mid-turn", Vec::new())
        .expect("queue mid-turn");

    // Steered messages are written to the transcript by the steer path, and the
    // queue lets go of them straight away.
    wait_for(
        || user_messages(&core).iter().any(|m| m == "mid-turn"),
        "the steered message to reach the transcript",
    )
    .await;
    assert!(queue_texts(&core).is_empty());

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Attachments never steer: the steer path carries a prompt and nothing else,
/// so a message with files waits for a turn that can inline them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_with_attachments_holds_even_for_a_steerable_agent() {
    let (core, harness, prompts) = setup(SteeringMode::StepBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "with a file", vec!["att-1".into()])
        .expect("queue attachment message");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue_texts(&core),
        vec!["with a file"],
        "a message carrying files must not be steered"
    );

    let _ = harness.finish.send(());
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "with a file"),
        "the held message to flush at turn end",
    )
    .await;

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// "Send now" (and the empty-composer Enter that pops the head) interrupts the
/// running turn and sends that one message, leaving the rest queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_now_interrupts_the_running_turn() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let first = core
        .doc_host
        .queue_message(CHAT, "urgent", Vec::new())
        .expect("queue urgent");
    core.doc_host
        .queue_message(CHAT, "later", Vec::new())
        .expect("queue later");

    assert!(
        core.doc_host
            .send_queued_now(CHAT, &first)
            .await
            .expect("send now"),
        "send now takes the row"
    );
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "urgent"),
        "the urgent message to reach the agent",
    )
    .await;
    assert_eq!(
        queue_texts(&core),
        vec!["later"],
        "the rest of the queue is untouched"
    );

    // A row someone else already took is not an error, just `false`.
    assert!(
        !core
            .doc_host
            .send_queued_now(CHAT, &first)
            .await
            .expect("second send now")
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Editing a queued message to nothing is the delete gesture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_a_queued_message_to_empty_removes_it() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let id = core
        .doc_host
        .queue_message(CHAT, "typo", Vec::new())
        .expect("queue typo");
    assert!(
        core.doc_host
            .update_queued_message(CHAT, &id, "fixed")
            .expect("edit")
    );
    assert_eq!(queue_texts(&core), vec!["fixed"]);

    assert!(
        core.doc_host
            .update_queued_message(CHAT, &id, "   ")
            .expect("empty edit")
    );
    assert!(
        queue_texts(&core).is_empty(),
        "emptying a queued message deletes it"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// Reordering, over the RPC surface the UIs actually call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_rpc_reorders_and_streams() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    let client = zeron_rpc::memory_client(core.rpc_service());

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;

    let mut rx = client
        .subscribe(
            zeron_rpc::methods::WATCH_QUEUE,
            serde_json::json!({ "chatId": CHAT }),
        )
        .await
        .expect("WatchQueue");
    let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("first queue frame")
        .expect("stream open");
    assert_eq!(
        first["items"].as_array().map(Vec::len),
        Some(0),
        "the stream opens with the current queue"
    );

    for text in ["a", "b", "c"] {
        client
            .call(
                zeron_rpc::methods::QUEUE_MESSAGE,
                serde_json::json!({ "chatId": CHAT, "text": text }),
            )
            .await
            .expect("QueueMessage");
    }
    assert_eq!(queue_texts(&core), vec!["a", "b", "c"]);

    let last_id = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_queue()
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    client
        .call(
            zeron_rpc::methods::MOVE_QUEUED_MESSAGE,
            serde_json::json!({ "chatId": CHAT, "id": last_id, "toIndex": 0 }),
        )
        .await
        .expect("MoveQueuedMessage");
    assert_eq!(queue_texts(&core), vec!["c", "a", "b"]);

    client
        .call(
            zeron_rpc::methods::REMOVE_QUEUED_MESSAGE,
            serde_json::json!({ "chatId": CHAT, "id": last_id }),
        )
        .await
        .expect("RemoveQueuedMessage");
    assert_eq!(queue_texts(&core), vec!["a", "b"]);

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// The regression the review caught: `drain_queue` runs from both the
/// doc-change task and the turn-end status watcher, and there is nothing about
/// those two callers that keeps them apart. Driven concurrently against an idle
/// agent, an unserialized drain has both take a different head across the
/// `dispatch` await and both send — the queue empties in one go, which is the
/// "looks sent" failure the whole feature exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_drains_release_one_message() {
    let (core, harness, prompts) = setup(SteeringMode::TurnBoundary).await;
    let handle = core.doc_host.open(CHAT).expect("open chat");

    for text in ["first", "second", "third"] {
        core.doc_host
            .queue_message(CHAT, text, Vec::new())
            .expect("queue");
    }

    tokio::join!(
        core.doc_host.drain_queue(&handle),
        core.doc_host.drain_queue(&handle),
        core.doc_host.drain_queue(&handle),
    );

    wait_for(
        || !prompts.lock().unwrap().is_empty(),
        "the released message to reach the agent",
    )
    .await;
    // Long enough for a second escapee to show up if the drains interleaved.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue_texts(&core),
        vec!["second", "third"],
        "one drain released the head; the others found a busy agent"
    );
    assert_eq!(
        prompts.lock().unwrap().clone(),
        vec!["first".to_string()],
        "the agent was handed exactly one prompt"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}

/// An agent parked on a question still owns the turn. The composer queues on
/// that state, so the drain has to hold there too — reading `AwaitingInput` as
/// idle sends the follow-up as a fresh turn and abandons the question.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_holds_while_the_agent_waits_on_a_question() {
    let (core, harness, prompts) = setup_asking(SteeringMode::TurnBoundary).await;

    core.doc_host
        .queue_message(CHAT, "opening", Vec::new())
        .expect("queue opening");
    wait_for(
        || prompts.lock().unwrap().iter().any(|p| p == "opening"),
        "the first turn to start",
    )
    .await;
    wait_for(
        || {
            core.sessions
                .session_status(CHAT)
                .is_some_and(|s| s.status == zeron_proto::SessionStatus::AwaitingInput)
        },
        "the agent to park on its question",
    )
    .await;

    core.doc_host
        .queue_message(CHAT, "follow-up", Vec::new())
        .expect("queue follow-up");
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        queue_texts(&core),
        vec!["follow-up"],
        "the follow-up waits for the question to be answered"
    );
    assert!(
        !prompts.lock().unwrap().iter().any(|p| p == "follow-up"),
        "no fresh turn started under the parked question"
    );

    let _ = harness.finish.send(());
    core.shutdown().await;
}
