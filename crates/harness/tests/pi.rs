//! Native Pi RPC integration tests against a deterministic fake CLI.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiNativeHarness, RunControls, SteerMessage};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, ToolCall,
    UserInputAnswer,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-pi.py");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> PiNativeHarness {
    PiNativeHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("openai-codex/gpt-5.6-sol".into()),
        reasoning: Some(ReasoningLevel::High),
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|questions| {
            let (tx, rx) = oneshot::channel();
            let answers = questions
                .into_iter()
                .map(|question| UserInputAnswer {
                    question_id: question.id,
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: interrupt.clone(),
    };
    (controls, steer_tx, interrupt)
}

async fn run_to_end(request: RunRequest, controls: RunControls) -> Vec<AgentEvent> {
    let stream = harness().run(request, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(5),
        stream.map(|event| event.expect("valid event")).collect(),
    )
    .await
    .expect("Pi stream settles")
}

#[tokio::test]
async fn happy_path_multiplexes_rpc_and_settles_without_duplicate_text() {
    let (controls, _steer, _interrupt) = controls();
    let events = run_to_end(request("happy"), controls).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted {
                harness: HarnessId::Pi,
                model,
                cwd,
                session_id,
                ..
            } if model == "openai-codex/gpt-5.6-sol"
                && cwd == "/tmp"
                && session_id == "/tmp/fake-pi-session.jsonl"
        )),
        "{events:?}"
    );

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "Hello after-agent-end",
        "completed messages must not duplicate streamed text: {events:?}"
    );

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCall { id, .. } if id == "call-1"))
            .count(),
        1,
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "call-1".into(),
        call: ToolCall::Exec {
            command: "printf hi".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "call-1".into(),
        is_error: false,
        output: Some("hi".into()),
        diff: None,
    }));
    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 12,
        output_tokens: 3
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
            .count(),
        2,
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { .. })),
        "{events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("/tmp/fake-pi-session.jsonl".into()),
        })
    );
}

#[tokio::test]
async fn message_end_is_a_fallback_when_no_delta_streamed() {
    let (controls, _steer, _interrupt) = controls();
    let events = run_to_end(request("fallback"), controls).await;
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, AgentEvent::TextDelta { text } if text == "non-streamed")
            )
            .count(),
        1,
        "{events:?}"
    );
}

#[tokio::test]
async fn subagent_tool_replays_child_transcript_as_nested_events() {
    let temp = tempfile::tempdir().unwrap();
    let transcript = temp
        .path()
        .join(".pi/subagents/artifacts/child_test_transcript.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let (controls, _steer, _interrupt) = controls();
    let events = run_to_end(
        request(&format!("subagent:{}", transcript.display())),
        controls,
    )
    .await;

    assert!(events.contains(&AgentEvent::ToolResult {
        id: "parent-subagent".into(),
        is_error: false,
        output: Some("Workflow completed".into()),
        diff: None,
    }));
    let nested: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Subagent {
                parent_tool_use_id,
                event,
            } if parent_tool_use_id == "parent-subagent" => Some(event.as_ref()),
            _ => None,
        })
        .collect();
    assert!(
        nested.contains(&&AgentEvent::UserMessage {
            text: "Inspect the fixture".into(),
        }),
        "{events:?}"
    );
    assert!(
        nested.contains(&&AgentEvent::ReasoningDelta {
            text: "Planning child work".into(),
        }),
        "{events:?}"
    );
    assert!(
        nested.contains(&&AgentEvent::ToolCall {
            id: "child-read".into(),
            call: ToolCall::ReadFile {
                path: "/tmp/input.txt".into(),
            },
        }),
        "{events:?}"
    );
    assert!(
        nested.contains(&&AgentEvent::ToolResult {
            id: "child-read".into(),
            is_error: false,
            output: Some("fixture body".into()),
            diff: None,
        }),
        "{events:?}"
    );
    assert!(
        nested.contains(&&AgentEvent::TextDelta {
            text: "Child finished".into(),
        }),
        "{events:?}"
    );
    assert!(
        nested.contains(&&AgentEvent::Usage {
            input_tokens: 7,
            output_tokens: 2,
        }),
        "{events:?}"
    );
    assert!(
        matches!(
            nested.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "{events:?}"
    );
    assert!(
        !nested.iter().any(|event| matches!(
            event,
            AgentEvent::UserMessage { text } if text == "[prompt redacted]"
        )),
        "{events:?}"
    );
    assert!(
        !nested.iter().any(|event| match event {
            AgentEvent::UserMessage { text }
            | AgentEvent::TextDelta { text }
            | AgentEvent::ReasoningDelta { text } =>
                text.contains("Acceptance Contract")
                    || text.contains("acceptance-report")
                    || text.contains("criteriaSatisfied"),
            _ => false,
        }),
        "{events:?}"
    );
}

#[tokio::test]
async fn steering_uses_pi_steer_and_rotates_message_boundary() {
    let (controls, steer, _interrupt) = controls();
    let stream = harness()
        .run(request("steer"), controls)
        .await
        .expect("run starts");
    steer
        .send(SteerMessage {
            prompt: "new direction".into(),
            message_id: None,
        })
        .await
        .unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        stream.map(|event| event.unwrap()).collect::<Vec<_>>(),
    )
    .await
    .expect("run settles");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "+steered:new direction".into()
        }),
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn interrupt_sends_abort_and_finishes_interrupted() {
    let (controls, _steer, interrupt) = controls();
    let stream = harness()
        .run(request("interrupt"), controls)
        .await
        .expect("run starts");
    interrupt.cancel();
    let events = tokio::time::timeout(
        Duration::from_secs(5),
        stream.map(|event| event.unwrap()).collect::<Vec<_>>(),
    )
    .await
    .expect("abort settles");
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Interrupted,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn resume_opens_session_at_process_start_without_switch_rpc() {
    let (controls, _steer, _interrupt) = controls();
    let mut req = request("fallback");
    req.resume = Some("/tmp/resumed-pi-session.jsonl".into());
    let events = run_to_end(req, controls).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { session_id, .. }
                if session_id == "/tmp/resumed-pi-session.jsonl"
        )),
        "{events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn model_and_command_discovery_use_documented_rpc_commands() {
    let models = harness().models().await.expect("models");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "openai-codex/gpt-5.6-sol");
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    );
    assert!(
        models[0]
            .description
            .as_deref()
            .unwrap()
            .contains("multimodal")
    );

    let commands = harness().commands().await.expect("commands");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "fix-tests");
    assert_eq!(commands[1].name, "skill:review");
}

#[tokio::test]
async fn failed_setup_command_becomes_terminal_error() {
    let (controls, _steer, _interrupt) = controls();
    let mut req = request("happy");
    req.model = Some("openai-codex/reject".into());
    let events = run_to_end(req, controls).await;
    assert!(
        matches!(events.last(), Some(AgentEvent::Done {
        status: DoneStatus::Errored,
        error: Some(error),
        ..
    }) if error.contains("Model not found: reject")),
        "{events:?}"
    );
}
