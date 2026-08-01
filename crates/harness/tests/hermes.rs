//! HermesHarness integration tests against the fake ACP server in
//! `tests/fixtures/fake-hermes.sh` (no real `hermes` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{CancellationToken, Harness, HermesHarness, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, TodoItem, ToolCall,
    UserInputAnswer,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-hermes.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> HermesHarness {
    HermesHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(200), Duration::from_millis(200))
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        // Differs from the fixture's currentModelId, so session/set_model must fire.
        model: Some("openai-codex:gpt-5.5".into()),
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

/// Controls whose `request_input` answers every question with `answer_label`.
fn controls(
    answer_label: &'static str,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec![answer_label.into()],
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

async fn run_to_end(
    harness: &HermesHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

/// Drop the steering sender so the persistent session reaps itself once the
/// turn is done, then collect.
async fn run_once(harness: &HermesHarness, req: RunRequest) -> Vec<AgentEvent> {
    let (controls, steer, _token) = controls("Yes");
    drop(steer);
    run_to_end(harness, req, controls).await
}

#[tokio::test]
async fn happy_path_maps_chunks_tool_calls_plan_usage_and_done() {
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    let events = run_once(&harness(), req).await;

    // SessionStarted carries the ACP session id and the requested model.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                session_id,
                ..
            } => Some((harness, model, cwd, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    let (h, model, cwd, session_id) = starts[0];
    assert_eq!(*h, HarnessId::Hermes);
    assert_eq!(model, "openai-codex:gpt-5.5");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "s-live");

    // Message vs thought chunks land on their own channels.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: " world".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking".into()
    }));

    // execute → Exec, resolved by its terminal update.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tc-1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tc-1".into(),
        is_error: false
    }));

    // read → ReadFile; a `failed` status is an error result.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tc-2".into(),
        call: ToolCall::ReadFile {
            path: "notes.txt".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tc-2".into(),
        is_error: true
    }));

    // A progress-only (`in_progress`) update resolves nothing at all.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { id, .. } if id == "tc-3")),
        "in_progress must not resolve: {events:?}"
    );

    // MCP prefix split.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tc-4".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "create_issue".into(),
            input: Some(serde_json::json!({"team": "eng"})),
        },
    }));

    // plan → a Todo call under one stable id.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "hermes-plan-s-live".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "Read the code".into(),
                    done: true
                },
                TodoItem {
                    text: "Write the fix".into(),
                    done: false
                },
            ]
        },
    }));

    // Usage comes from the prompt RESPONSE, not the context-window gauge.
    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 16165,
        output_tokens: 20,
    }));

    // Exactly one Done, and it closes the stream.
    let dones: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Done { .. }))
        .collect();
    assert_eq!(dones.len(), 1, "{events:?}");
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id: Some(id),
            ..
        }) if id == "s-live"
    ));
}

/// `session/load` replays the whole prior transcript before responding. Comet's
/// doc already holds those parts, so not one replayed event may escape.
#[tokio::test]
async fn resume_replays_are_swallowed() {
    let mut req = request("scenario:resumed");
    req.resume = Some("s-resumed".into());
    let events = run_once(&harness(), req).await;

    assert!(
        !events.contains(&AgentEvent::TextDelta {
            text: "REPLAYED-ASSISTANT-TEXT".into()
        }),
        "replayed history leaked into the stream: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "tc-replay")),
        "replayed tool call leaked: {events:?}"
    );
    // The live turn after the resume still streams.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "after resume".into()
    }));
    assert!(matches!(
        events.first(),
        Some(AgentEvent::SessionStarted { session_id, .. }) if session_id == "s-resumed"
    ));
}

/// A session Hermes cannot find answers `null`; the harness starts fresh
/// instead of failing the run.
#[tokio::test]
async fn unknown_resume_falls_back_to_a_fresh_session() {
    let mut req = request("scenario:happy");
    req.resume = Some("resume-fail".into());
    req.cwd = "/tmp".into();
    let events = run_once(&harness(), req).await;

    assert!(matches!(
        events.first(),
        Some(AgentEvent::SessionStarted { session_id, .. }) if session_id == "s-fresh"
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

/// Comet's sandbox level picks Hermes's edit-approval mode; the fixture asserts
/// the wire value and fails the turn if it is wrong.
#[tokio::test]
async fn read_only_without_auto_approve_selects_the_default_mode() {
    let mut req = request("scenario:readonly");
    req.sandbox = SandboxLevel::ReadOnly;
    req.auto_approve = false;
    let events = run_once(&harness(), req).await;
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

/// `session/request_permission` round-trips through `request_input`; a "Yes"
/// selects the allow option, a "No" the reject one.
#[tokio::test]
async fn permission_requests_bridge_to_request_input() {
    for (answer, expected) in [("Yes", "picked:allow_once"), ("No", "picked:deny")] {
        let (controls, steer, _token) = controls(answer);
        drop(steer);
        let mut req = request("scenario:permission");
        req.auto_approve = false;
        let events = run_to_end(&harness(), req, controls).await;
        assert!(
            events.contains(&AgentEvent::TextDelta {
                text: expected.into()
            }),
            "answering {answer} should send {expected}: {events:?}"
        );
    }
}

/// `auto_approve` allows without ever asking the user.
#[tokio::test]
async fn auto_approve_allows_without_consulting_the_user() {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    drop(steer_tx);
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = asked.clone();
    let controls = RunControls {
        request_input: Box::new(move |_questions| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: CancellationToken::new(),
    };
    let events = run_to_end(&harness(), request("scenario:permission"), controls).await;
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "auto_approve must not ask"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "picked:allow_once".into()
    }));
}

/// A steer sent mid-turn is absorbed by the running turn: Comet emits `Steered`,
/// swallows Hermes's "Redirected…" acknowledgement, and still ends with exactly
/// ONE Done — the ack response is not a turn end.
#[tokio::test]
async fn mid_turn_steer_emits_steered_and_swallows_the_ack() {
    let (controls, steer_tx, _token) = controls("Yes");
    let stream = harness()
        .run(request("scenario:steer"), controls)
        .await
        .expect("run starts");
    tokio::pin!(stream);

    let mut events: Vec<AgentEvent> = Vec::new();
    let collect = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            // Steer only once the first chunk proves the turn is live.
            if ev
                == (AgentEvent::TextDelta {
                    text: "first".into(),
                })
            {
                steer_tx
                    .send(SteerMessage {
                        prompt: "steered text".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer accepted");
                // Close the mailbox so the session reaps after the turn.
                drop(steer_tx.clone());
            }
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("run finished in time");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(
        !events.contains(&AgentEvent::TextDelta {
            text: "Redirected the active turn with your correction.".into()
        }),
        "the steer ack must not reach the transcript: {events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "second".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Done { .. }))
            .count(),
        1,
        "the ack response must not be mistaken for a turn end: {events:?}"
    );
}

/// Cancelling the token sends `session/cancel`; the pending prompt resolves
/// `cancelled` and the stream ends Interrupted.
#[tokio::test]
async fn interrupt_cancels_the_session_and_ends_interrupted() {
    let (controls, _steer, token) = controls("Yes");
    let stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    tokio::pin!(stream);

    let mut events: Vec<AgentEvent> = Vec::new();
    let collect = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if ev
                == (AgentEvent::TextDelta {
                    text: "working".into(),
                })
            {
                token.cancel();
            }
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("run finished in time");

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

/// Interrupting a persistent session that is idle BETWEEN turns still closes
/// the stream with a Done — the "never end without a Done after an interrupt"
/// contract the Codex harness also holds.
#[tokio::test]
async fn interrupt_while_idle_between_turns_still_ends_with_done() {
    // Keep the steering mailbox open so the session survives its first turn.
    let (controls, _steer_tx, token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    let stream = harness().run(req, controls).await.expect("run starts");
    tokio::pin!(stream);

    let mut dones = 0usize;
    let mut statuses = Vec::new();
    let collect = async {
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done { status, .. } = ev.expect("stream event") {
                dones += 1;
                statuses.push(status);
                if dones == 1 {
                    // The turn is over but the session is still alive.
                    token.cancel();
                } else {
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("run finished in time");

    assert_eq!(statuses.first(), Some(&DoneStatus::Completed));
    assert_eq!(
        statuses.get(1),
        Some(&DoneStatus::Interrupted),
        "an idle interrupt must still emit a terminal Done: {statuses:?}"
    );
}

/// A JSON-RPC error on `session/prompt` ends the run as Errored, carrying the
/// server's message rather than a silent success.
#[tokio::test]
async fn prompt_error_ends_the_run_errored() {
    let events = run_once(&harness(), request("scenario:promptfail")).await;
    let Some(AgentEvent::Done {
        status,
        error: Some(error),
        ..
    }) = events.last()
    else {
        panic!("expected an errored Done: {events:?}");
    };
    assert_eq!(*status, DoneStatus::Errored);
    assert!(error.contains("provider exploded"), "{error}");
}

/// The live catalog comes from `session/new`'s `models.availableModels`, and is
/// served from cache on the second call.
#[tokio::test]
async fn models_are_discovered_live_and_cached() {
    let harness = harness();
    let models = harness.models().await.expect("models discovered");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "xai-oauth:grok-4.5");
    assert_eq!(models[0].label, "xAI · grok-4.5");
    assert_eq!(models[0].description.as_deref(), Some("Provider: xAI"));
    // Hermes exposes no per-turn effort knob.
    assert!(models[0].reasoning_levels.is_empty());
    assert!(models[1].description.is_none());

    let again = harness.models().await.expect("cached models");
    assert_eq!(again, models);
}

/// End-to-end against the REAL `hermes acp`. Ignored by default: it needs an
/// installed Hermes with a configured provider and hits the network. Run with
/// `cargo test -p comet-harness --test hermes -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "requires an installed, provider-configured hermes CLI + network"]
async fn live_hermes_cli_streams_a_real_turn() {
    let harness = HermesHarness::new();

    let models = harness.models().await.expect("live model discovery");
    assert!(!models.is_empty(), "a configured hermes reports models");
    eprintln!("discovered {} models, first: {:?}", models.len(), models[0]);

    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, steer, _token) = controls("Yes");
    drop(steer);
    let mut req = request("Reply with exactly: PONG");
    req.model = None; // whatever the device is configured for
    req.cwd = dir.path().display().to_string();

    let stream = harness.run(req, controls).await.expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(240),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("live turn finished in time");

    assert!(
        matches!(events.first(), Some(AgentEvent::SessionStarted { .. })),
        "{events:?}"
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("assistant text: {text:?}");
    assert!(text.contains("PONG"), "expected PONG, got {text:?}");
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

/// A missing binary is a typed NotInstalled, never a panic or a hang.
#[tokio::test]
async fn missing_binary_reports_not_installed() {
    let harness = HermesHarness::new().with_executable("/nonexistent/hermes");
    assert!(matches!(
        harness.models().await,
        Err(comet_harness::HarnessError::NotInstalled(_))
    ));
    let (controls, _steer, _token) = controls("Yes");
    assert!(matches!(
        harness.run(request("x"), controls).await.err(),
        Some(comet_harness::HarnessError::NotInstalled(_))
    ));
}
