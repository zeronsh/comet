//! Regression tests for #296: quiet output never relinquishes a pending prompt.
//! The retired env knob stays set so reintroducing its old behavior fails fast.

use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

const QUIET_MS: u64 = 1200;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set before any harness runs in this test process; all
        // tests in this binary share the one value.
        unsafe { std::env::set_var("ZERON_ACP_QUIET_SETTLE_MS", QUIET_MS.to_string()) };
    });
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("acp-lifecycle.py");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("grok-4.5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

async fn collect_until_done(
    stream: &mut futures::stream::BoxStream<
        'static,
        Result<AgentEvent, zeron_harness::HarnessError>,
    >,
) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.expect("stream event");
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("turn must settle")
}

fn assert_done(events: &[AgentEvent], expected: DoneStatus) {
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(dones.len(), 1, "{events:?}");
    assert_eq!(dones[0].0, expected, "{events:?}");
    if expected != DoneStatus::Errored {
        assert_eq!(dones[0].1, None);
    }
}

async fn delayed_turn(scenario: &str) {
    init_env();
    let harness = AcpHarness::pi().with_executable(fixture_path());
    assert!(harness.authoritative_prompt_end());
    let (controls, steer, token) = controls();
    let mut stream = harness.run(request(scenario), controls).await.unwrap();
    // Queue multiple follow-ups while the first prompt remains outstanding.
    steer
        .send(SteerMessage {
            message_id: None,
            prompt: "second".into(),
        })
        .await
        .unwrap();
    steer
        .send(SteerMessage {
            message_id: None,
            prompt: "third".into(),
        })
        .await
        .unwrap();
    let first = collect_until_done(&mut stream).await;
    assert_done(&first, DoneStatus::Completed);
    assert!(
        first
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "finished")),
        "premature Done: {first:?}"
    );
    for expected in ["second", "third"] {
        let events = collect_until_done(&mut stream).await;
        assert_done(&events, DoneStatus::Completed);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == expected)),
            "{events:?}"
        );
    }
    // A fresh user message after completion also reuses the same session.
    steer
        .send(SteerMessage {
            message_id: None,
            prompt: "fourth".into(),
        })
        .await
        .unwrap();
    let fourth = collect_until_done(&mut stream).await;
    assert_done(&fourth, DoneStatus::Completed);
    assert!(
        fourth
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "fourth"))
    );
    drop(steer);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    drop(token);
}

#[tokio::test]
async fn completed_tools_then_silence_preserves_prompt_and_followups() {
    delayed_turn("tools").await;
}
#[tokio::test]
async fn partial_text_then_silence_preserves_prompt_and_followups() {
    delayed_turn("text").await;
}
#[tokio::test]
async fn usage_is_not_completion() {
    delayed_turn("usage").await;
}
#[tokio::test]
async fn reasoning_then_silence_preserves_prompt() {
    delayed_turn("reasoning").await;
}
#[tokio::test]
async fn open_tools_then_silence_preserves_prompt() {
    delayed_turn("open-tool").await;
}

async fn cancel_quiet(scenario: &str) {
    init_env();
    let (controls, steer, token) = controls();
    let mut stream = AcpHarness::pi()
        .with_executable(fixture_path())
        .run(request(scenario), controls)
        .await
        .unwrap();
    while let Some(e) = stream.next().await {
        if matches!(e.unwrap(), AgentEvent::ToolResult { id, .. } if id == "3") {
            break;
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(QUIET_MS * 2), stream.next())
            .await
            .is_err(),
        "silence must not emit Done"
    );
    steer
        .send(SteerMessage {
            message_id: None,
            prompt: "must not run".into(),
        })
        .await
        .unwrap();
    token.cancel();
    let events = collect_until_done(&mut stream).await;
    assert_done(&events, DoneStatus::Interrupted);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { .. }))
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn quiet_turn_can_be_cancelled_without_promoting_followup() {
    cancel_quiet("cancel").await;
}
#[tokio::test]
async fn unresponsive_quiet_turn_is_killed_on_cancel() {
    cancel_quiet("wedge").await;
}

#[tokio::test]
async fn missing_response_at_eof_is_error_not_success() {
    init_env();
    let (controls, _steer, _token) = controls();
    let mut stream = AcpHarness::pi()
        .with_executable(fixture_path())
        .run(request("eof"), controls)
        .await
        .unwrap();
    assert_done(&collect_until_done(&mut stream).await, DoneStatus::Errored);
}

#[tokio::test]
async fn protocol_error_keeps_code_and_agent_detail() {
    let (controls, _steer, _token) = controls();
    let mut stream = AcpHarness::pi()
        .with_executable(fixture_path())
        .run(request("error"), controls)
        .await
        .unwrap();
    let events = collect_until_done(&mut stream).await;
    assert_done(&events, DoneStatus::Errored);
    let error = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Done { error, .. } => error.as_deref(),
            _ => None,
        })
        .unwrap();
    for expected in [
        "session/prompt",
        "Invalid request",
        "-32600",
        "A prompt is already running",
        "retryable",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}
