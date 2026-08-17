//! AcpHarness integration tests against the fake ACP agent in
//! `tests/fixtures/fake-acp.sh` (no real `grok` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{
    AcpHarness, CancellationToken, Harness, HarnessError, RunControls, SteerMessage,
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, SteeringMode, TodoItem, ToolCall,
    UserInputAnswer,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> AcpHarness {
    AcpHarness::grok().with_executable(fixture_path())
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
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
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

async fn run_to_end(
    harness: &AcpHarness,
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

fn dones(events: &[AgentEvent]) -> Vec<(DoneStatus, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, error, .. } => Some((*status, error.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn happy_path_maps_chunks_tools_diffs_plans_and_commands() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;

    // SessionStarted from session/new's id.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { harness, session_id, cwd, .. }
                if *harness == HarnessId::Grok && session_id == "s-1" && cwd == "/tmp"
        )),
        "{events:?}"
    );

    // Initialize-advertised commands surface before the turn.
    let commands: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AvailableCommands { commands } => Some(commands.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(commands.len(), 2, "{events:?}");
    assert_eq!(commands[0][0].name, "compact");
    assert_eq!(commands[0][1].input_hint.as_deref(), Some("the goal"));
    // Mid-run advertisement replaces the list.
    assert_eq!(commands[1][0].name, "deep-research");

    // Chunks; the wrong-session and non-text chunks never surface.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking".into()
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("WRONG"))),
        "{events:?}"
    );

    // Execute tool: pending opens the call, the completed update resolves it
    // with capped multi-line output (newlines preserved verbatim).
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t1".into(),
        call: ToolCall::Exec {
            command: "cargo test -p zeron-harness".into()
        },
    }));
    let exec_output = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error: false,
                output: Some(output),
                ..
            } if id == "t1" => Some(output.clone()),
            _ => None,
        })
        .expect("exec output present");
    assert!(exec_output.starts_with("   Compiling zeron-harness"));
    assert_eq!(exec_output.lines().count(), 6, "{exec_output:?}");

    // Edit tool: single-shot completed call carries the inline diff.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "t2".into(),
        call: ToolCall::EditFile {
            path: "/w/src/resolve.rs".into(),
            old_string: None,
            new_string: None,
        },
    }));
    let diff = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                diff: Some(diff),
                ..
            } if id == "t2" => Some(diff.clone()),
            _ => None,
        })
        .expect("edit diff present");
    assert_eq!(diff.path, "/w/src/resolve.rs");
    assert!(
        diff.old_text
            .as_deref()
            .is_some_and(|t| t.contains(".filter(|p| p.exists())")),
        "{diff:?}"
    );
    assert!(diff.new_text.contains("split_paths"), "{diff:?}");

    // Plan → stable todo chip.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "acp-plan".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "read".into(),
                    done: true
                },
                TodoItem {
                    text: "fix".into(),
                    done: false
                },
            ]
        },
    }));

    // usage_update maps to nothing (context gauge, not per-turn tokens).
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Usage { .. })));

    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn config_options_apply_requested_model_and_effort() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:config");
    req.reasoning = Some(zeron_proto::ReasoningLevel::Medium);
    let events = run_to_end(&harness(), req, controls).await;
    // The fixture answers refusal unless BOTH set_config_option calls
    // (model grok-4.5, effort medium) arrived before the prompt.
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "configured".into()
        }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn question_shaped_requests_bridge_to_the_input_panel() {
    // The controls' bridge answers every question with its FIRST option
    // label — build controls that answer "Use tokio" specifically.
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Use tokio".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let _keep = (steer_tx, token);
    let events = run_to_end(&harness(), request("scenario:question"), controls).await;
    // The fixture answers refusal unless the harness relayed the choice
    // (optionId opt-tokio) instead of auto-accepting.
    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "answered".into()
        }),
        "{events:?}"
    );
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn claude_and_codex_specs_drive_the_same_wire() {
    // The whole point of the conversion: every spec runs against the same
    // fake ACP agent with no per-agent protocol code. Model ids in the
    // fixture are grok-flavored, so config sets simply skip.
    for (name, h) in [
        (
            "claude",
            AcpHarness::claude().with_executable(fixture_path()),
        ),
        ("codex", AcpHarness::codex().with_executable(fixture_path())),
        (
            "hermes",
            AcpHarness::hermes().with_executable(fixture_path()),
        ),
        ("pi", AcpHarness::pi().with_executable(fixture_path())),
        (
            "cursor",
            AcpHarness::cursor().with_executable(fixture_path()),
        ),
    ] {
        let (controls, _steer, _token) = controls();
        let events = run_to_end(&h, request("scenario:happy"), controls).await;
        assert!(
            events.contains(&AgentEvent::TextDelta {
                text: "Hello".into()
            }),
            "{name}: {events:?}"
        );
        assert_eq!(
            dones(&events),
            vec![(DoneStatus::Completed, None)],
            "{name}"
        );
    }
}

#[tokio::test]
async fn ultrathink_prefixes_the_prompt_for_claude() {
    let (controls, _steer, _token) = controls();
    let h = AcpHarness::claude().with_executable(fixture_path());
    let mut req = request("scenario:echo-prompt");
    req.reasoning = Some(zeron_proto::ReasoningLevel::Ultrathink);
    let events = run_to_end(&h, req, controls).await;
    // The fixture echoes the prompt text back; the Ultrathink prefix must be
    // on the wire.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.starts_with("Ultrathink:")
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn permission_requests_auto_accept_the_preferred_allow_option() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:permission"), controls).await;
    // The fixture answers refusal unless the harness selected "always".
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "approved".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn steering_extension_injects_mid_turn() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-ext"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first") {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

/// The steering response racing the turn's own end: the injection landed in
/// the dying turn, and the prompt response reached the wire first. The
/// boundary must still be emitted BEFORE the Done — a Steered after Done
/// re-armed the consumer (parked session → Working) with no next turn and no
/// Done ever coming (the stranded-Working / eternal-timer bug).
#[tokio::test]
async fn steer_racing_the_turn_end_never_emits_steered_after_done() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-race"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first") {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("steer landed in the turn: a Steered boundary must exist");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("checked above");
    assert!(
        steered < done,
        "Steered after Done strands the session: {events:?}"
    );
}

#[tokio::test]
async fn rejected_steer_queues_and_delivers_at_the_turn_boundary() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer-queue"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first")
                && let Some(steer) = &steer
            {
                steer
                    .send(SteerMessage {
                        prompt: "redirect please".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            // Close the mailbox once the boundary turn streams so the
            // persistent session winds down and the stream ends.
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "boundary") {
                steer = None;
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // First turn completes, then the queued steer becomes the boundary turn.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "boundary".into()
    }));
}

#[tokio::test]
async fn interrupt_sends_session_cancel_and_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");
    assert_eq!(dones(&events), vec![(DoneStatus::Interrupted, None)]);
}

#[tokio::test]
async fn wedged_agent_escalates_to_signals_and_still_ends_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness().with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("escalation reaped the child in time");
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    assert_eq!(dones[0].0, DoneStatus::Interrupted);
}

#[tokio::test]
async fn refusal_maps_to_an_errored_done() {
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:refusal"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1);
    assert_eq!(dones[0].0, DoneStatus::Errored);
    assert!(dones[0].1.as_deref().unwrap_or("").contains("refused"));
}

#[tokio::test]
async fn resume_loads_the_session_and_drops_replayed_history() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:resumed");
    req.resume = Some("s-loaded".into());
    let events = run_to_end(&harness(), req, controls).await;
    // The 600-update replay is drained without surfacing…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("old reply"))),
        "{events:?}"
    );
    // …the loaded session id sticks, and the live turn still streams.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "s-loaded"
    )));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "back again".into()
    }));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn failed_load_falls_back_to_a_fresh_session() {
    let (controls, _steer, _token) = controls();
    let mut req = request("scenario:resumed");
    req.resume = Some("load-fail".into());
    let events = run_to_end(&harness(), req, controls).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "s-fresh"
    )));
    assert_eq!(dones(&events), vec![(DoneStatus::Completed, None)]);
}

#[tokio::test]
async fn commands_discovery_scans_the_initialize_response() {
    let harness = harness();
    let commands = harness.commands().await.expect("discovery");
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0].name, "compact");
    assert_eq!(commands[1].name, "goal");
    assert_eq!(commands[1].input_hint.as_deref(), Some("the goal"));
    // Cached: a second call must not respawn (same result, instant).
    let again = harness.commands().await.expect("cached");
    assert_eq!(again, commands);
}

#[tokio::test]
async fn missing_binary_surfaces_not_installed_with_install_hint() {
    let harness = AcpHarness::grok().with_executable("/nonexistent/definitely-not-grok");
    let err = harness
        .run(request("x"), controls().0)
        .await
        .err()
        .expect("missing binary must fail");
    assert!(matches!(
        err,
        HarnessError::NotInstalled(_) | HarnessError::Io(_)
    ));
}

/// Real-adapter smoke: spawns the actual `claude-agent-acp` (via npx when not
/// Discovery against the real installed adapters: base model rows only
/// (never one per reasoning effort), with wire-derived trait options. Free
/// (initialize + session/new, no prompt), but needs the CLIs installed and
/// authenticated. Run explicitly:
/// `cargo test -p zeron-harness --test acp -- --ignored real_discovery`
#[tokio::test]
#[ignore = "needs the claude + codex CLIs installed and authenticated"]
async fn real_discovery_yields_base_models_with_traits() {
    let codex = AcpHarness::codex().models().await.expect("codex discovery");
    assert!(!codex.is_empty());
    for m in &codex {
        assert!(
            !m.id.contains('[') || m.id.ends_with("[1m]"),
            "effort-variant leaked as a model row: {}",
            m.id
        );
    }
    let sol = codex.iter().find(|m| m.id == "gpt-5.6-sol").expect("sol");
    assert!(
        sol.options.iter().any(|o| o.id == "fast-mode"),
        "codex fast-mode trait missing: {:?}",
        sol.options
    );
    assert!(!sol.reasoning_levels.is_empty());

    let claude = AcpHarness::claude()
        .models()
        .await
        .expect("claude discovery");
    assert!(!claude.is_empty());
    for m in &claude {
        assert!(
            !m.reasoning_levels.is_empty(),
            "claude ladder missing on {}",
            m.id
        );
        assert!(
            m.reasoning_levels
                .contains(&zeron_proto::ReasoningLevel::Ultrathink),
            "ultrathink extra missing on {}",
            m.id
        );
    }
}

/// installed) against the installed, authenticated claude CLI and burns one
/// tiny haiku prompt. Run explicitly:
/// `cargo test -p zeron-harness --test acp -- --ignored real_claude`
#[tokio::test]
#[ignore = "needs the claude CLI authenticated + network; costs one tiny prompt"]
async fn real_claude_adapter_end_to_end() {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::claude();
    let mut req = request("Reply with exactly the word ACP-OK and nothing else.");
    req.model = Some("claude-haiku-4-5".into());
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let stream = harness.run(req, controls).await.expect("run starts");
    // The session parks after Done while the steering mailbox lives (the
    // engine reaps by dropping it) — release the sender at Done or the
    // stream never ends.
    let events = tokio::time::timeout(Duration::from_secs(180), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("ACP-OK"),
        "unexpected reply: {text:?}\n{events:?}"
    );
    assert_eq!(dones(&events).len(), 1, "{events:?}");
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
}

/// The Cursor slot against the real `cursor-agent acp` server: discovery
/// (free) plus one tiny prompt. Run explicitly:
/// `cargo test -p zeron-harness --test acp -- --ignored real_cursor`
#[tokio::test]
#[ignore = "needs the cursor-agent CLI authenticated + network; costs one tiny prompt"]
async fn real_cursor_adapter_end_to_end() {
    // `session/new` is the source of truth for models — the static catalog is
    // a fallback, so a live account must produce more than it.
    let models = AcpHarness::cursor().models().await.expect("discovery");
    assert!(models.len() > 5, "{models:?}");
    assert!(
        models
            .iter()
            .any(|m| m.id == "composer-2.5" || m.id.starts_with("composer-2.5")),
        "{models:?}"
    );
    // Parameterized picker: base ids, no raw HTML, Mode on every row, Auto
    // exposes Intelligence / Balance / Cost.
    assert!(
        models.iter().all(|m| !m.label.contains('<')),
        "html leaked into labels: {:?}",
        models
            .iter()
            .filter(|m| m.label.contains('<'))
            .map(|m| &m.label)
            .collect::<Vec<_>>()
    );
    assert!(
        models
            .iter()
            .all(|m| m.options.iter().any(|o| o.id == "mode")),
        "Mode trait missing"
    );
    let auto = models
        .iter()
        .find(|m| m.id == "auto-smart")
        .expect("parameterized Auto id");
    let optimize = auto
        .options
        .iter()
        .find(|o| o.id == "optimize_for")
        .expect("Optimize For");
    let tiers: Vec<&str> = optimize.choices.iter().map(|c| c.id.as_str()).collect();
    assert!(tiers.contains(&"intelligence"), "{tiers:?}");
    assert!(tiers.contains(&"balanced"), "{tiers:?}");
    assert!(tiers.contains(&"cost"), "{tiers:?}");

    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::cursor();
    let mut req = request("Reply with exactly the word ACP-OK and nothing else.");
    req.model = models
        .iter()
        .find(|m| m.id == "composer-2.5" || m.id.starts_with("composer-2.5"))
        .map(|m| m.id.clone());
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let stream = harness.run(req, controls).await.expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(180), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("ACP-OK"), "unexpected reply: {text:?}");
    assert_eq!(dones(&events).len(), 1, "{events:?}");
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
}

/// Cursor's todo extension reaches the stream as a chip, and its tools land
/// through the standard `session/update` path. Worth pinning live: the docs
/// call `cursor/update_todos` a fire-and-forget notification, but the CLI
/// sends it as a REQUEST — an unanswered one would stall the turn. Run:
/// `cargo test -p zeron-harness --test acp -- --ignored real_cursor_todos`
#[tokio::test]
#[ignore = "needs the cursor-agent CLI authenticated + network; costs one small prompt"]
async fn real_cursor_todos_and_tools_reach_the_stream() {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::cursor();
    let mut req = request(
        "Use your todo tool to record 3 steps, then run `echo hi` in the shell. \
         Keep it brief.",
    );
    req.reasoning = None;
    req.cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let stream = harness.run(req, controls).await.expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(240), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) {
                steer = None;
            }
            events.push(ev);
        }
        drop(steer);
        events
    })
    .await
    .expect("real run finished in time");
    let todos: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCall {
                    call: zeron_proto::ToolCall::Todo { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(!todos.is_empty(), "no todo chip: {events:?}");
    // Repeated updates refresh one chip rather than stacking new ones.
    assert!(
        todos.iter().all(|e| matches!(
            e,
            AgentEvent::ToolCall { id, .. } if id == "cursor-todos"
        )),
        "todo chips must share the stable id: {todos:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall {
                call: zeron_proto::ToolCall::Exec { .. },
                ..
            }
        )),
        "no exec tool call: {events:?}"
    );
    assert_eq!(dones(&events)[0].0, DoneStatus::Completed, "{events:?}");
}

#[test]
fn descriptor_surface_matches_registry_expectations() {
    let harness = AcpHarness::grok();
    assert_eq!(harness.id(), HarnessId::Grok);
    assert_eq!(harness.display_name(), "Grok");
    assert!(harness.supports_steering());
    assert_eq!(harness.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        harness.reasoning_levels(),
        &[
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
        ]
    );
}

#[tokio::test]
async fn models_are_discovered_from_the_acp_session() {
    // ACP is the source of truth: the fixture advertises a model config
    // option, so the picker list comes from the wire, not the static catalog.
    let harness = AcpHarness::hermes().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["grok-4-fast", "grok-4.5"], "{models:?}");
    // Unmatched ids inherit the probe session's thought_level ladder.
    assert_eq!(
        models[0].reasoning_levels,
        vec![
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
        ],
        "{models:?}"
    );
    assert_eq!(models[0].description.as_deref(), Some("Fast tier"));
    // Cached: a second call returns the same list without respawning.
    let again = harness.models().await.expect("cached");
    assert_eq!(again, models);
}

#[tokio::test]
async fn models_enrich_from_the_static_catalog_on_id_match() {
    // grok's static catalog knows "grok-4.5" — the discovered entry keeps the
    // wire label but inherits the curated description and ladder.
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let models = harness.models().await.expect("discovery");
    let grok45 = models
        .iter()
        .find(|m| m.id == "grok-4.5")
        .expect("grok-4.5");
    assert_eq!(
        grok45.description.as_deref(),
        Some("xAI's coding model — 500k context"),
        "{grok45:?}"
    );
}

#[tokio::test]
async fn models_fall_back_to_the_static_catalog_when_the_probe_fails() {
    let harness = AcpHarness::pi().with_executable("/nonexistent/never-a-pi-acp");
    let models = harness.models().await.expect("static fallback");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["default"], "{models:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn hung_handshake_errors_instead_of_spinning_forever() {
    // An agent that consumes stdin and never answers initialize — the
    // "thinking for minutes, then nothing" startup class (issue #93). The
    // run must end with a Done that names the timeout, not hang.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hung-agent.sh");
    // sleep inherits the stdio pipes and holds them open without ever
    // answering — a true wedge, not a crash.
    std::fs::write(&script, "#!/bin/sh\nexec sleep 1000\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let harness = AcpHarness::grok()
        .with_executable(&script)
        .with_handshake_timeout(Duration::from_millis(300));
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness, request("hi"), controls).await;
    let dones = dones(&events);
    assert_eq!(dones.len(), 1, "{events:?}");
    let (status, error) = &dones[0];
    assert_eq!(*status, DoneStatus::Errored);
    let error = error.as_deref().unwrap_or_default();
    assert!(
        error.contains("did not complete the ACP handshake"),
        "{error}"
    );
}

#[test]
fn cursor_descriptor_surface_matches_registry_expectations() {
    let cursor = AcpHarness::cursor();
    assert_eq!(cursor.id(), HarnessId::Cursor);
    assert_eq!(cursor.display_name(), "Cursor");
    assert!(cursor.supports_steering());
    assert_eq!(cursor.steering_mode(), SteeringMode::TurnBoundary);
    // Cursor carries effort in the model id's bracket suffix, so there is no
    // separate ladder for the Reasoning dropdown to drive.
    assert!(cursor.reasoning_levels().is_empty());
}

#[test]
fn hermes_and_pi_descriptor_surfaces_match_registry_expectations() {
    let hermes = AcpHarness::hermes();
    assert_eq!(hermes.id(), HarnessId::Hermes);
    assert_eq!(hermes.display_name(), "Hermes");
    assert!(hermes.supports_steering());
    assert_eq!(hermes.steering_mode(), SteeringMode::TurnBoundary);
    assert!(hermes.reasoning_levels().is_empty());

    let pi = AcpHarness::pi();
    assert_eq!(pi.id(), HarnessId::Pi);
    assert_eq!(pi.display_name(), "Pi");
    assert!(pi.supports_steering());
    assert_eq!(pi.steering_mode(), SteeringMode::TurnBoundary);
    assert_eq!(
        pi.reasoning_levels(),
        &[
            zeron_proto::ReasoningLevel::Minimal,
            zeron_proto::ReasoningLevel::Low,
            zeron_proto::ReasoningLevel::Medium,
            zeron_proto::ReasoningLevel::High,
            zeron_proto::ReasoningLevel::XHigh,
            zeron_proto::ReasoningLevel::Max,
        ]
    );
}

/// The 2026-08-12 stuck-Working wedge, end to end: a prompt whose turn was
/// consumed by CLI-side self-continuation never gets its response. A steer's
/// `noRunningTurn` steering outcome is the protocol evidence the pending
/// prompt can never settle; after the grace the harness closes the dead turn
/// (Done — never a stranded Working) and promotes the steer to a fresh
/// prompt, which settles normally.
#[tokio::test]
async fn starved_prompt_recovers_via_no_running_turn_evidence() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:starve"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "working") {
                steer
                    .send(SteerMessage {
                        prompt: "what about now".into(),
                        message_id: None,
                    })
                    .await
                    .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // Two settled turns: the synthesized close of the starved prompt, then
    // the promoted steer's real turn.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "promoted".into()
    }));
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("the queued steer must be promoted through a Steered boundary");
    let first_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("dones asserted above");
    assert!(
        first_done < steered,
        "the dead turn settles before the promoted boundary: {events:?}"
    );
}

/// The dropped-reply turn end with no steer involved: the adapter emits the
/// turn's terminal cost frame (`usage_update` with `cost`) but never the
/// prompt response. The claude spec must settle the turn off that evidence
/// within its 1s grace — Working clears in about a second, not never (and
/// not only after a watchdog window).
#[tokio::test]
async fn dropped_reply_settles_fast_off_the_turn_end_cost_frame() {
    let (controls, _steer, _token) = controls();
    let harness = AcpHarness::claude().with_executable(fixture_path());
    let started = std::time::Instant::now();
    let stream = harness
        .run(request("scenario:cost-starve"), controls)
        .await
        .expect("run starts");
    let (events, done_at) = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        let mut done_at = None;
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::Done { .. }) && done_at.is_none() {
                done_at = Some(started.elapsed());
            }
            events.push(ev);
        }
        (events, done_at)
    })
    .await
    .expect("run finished in time");

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "{events:?}"
    );
    // The fixture holds its stream open for 6s after the cost frame; the
    // Done must come from the 1s cost-hint grace, not stream EOF (margin
    // sized for parallel-suite load).
    let done_at = done_at.expect("dones asserted above");
    assert!(
        done_at < Duration::from_secs(4),
        "Done at {done_at:?} — should be ~1s after the cost frame, well \
         before the fixture's 6s exit"
    );
}

/// Full-stack verification of the 2026-08-12 starve fix against the REAL
/// adapter + CLI: prompt#1 backgrounds a task and ends; the CLI
/// self-continues on its notification and runs a 20s foreground command; a
/// steer lands mid-way. With prevention in place the harness cancels the
/// unowned turn and prompts fresh (no starve to recover from); the turn
/// must settle promptly either way — never strand, never wait for a
/// watchdog. Costs a few small prompts. Run explicitly:
/// `cargo test -p zeron-harness --test acp -- --ignored real_claude_starve`
#[tokio::test]
#[ignore = "needs the claude CLI authenticated + network; costs a few small prompts"]
async fn real_claude_starve_settles_off_the_cost_frame() {
    let (controls, steer_tx, _token) = controls();
    let harness = AcpHarness::claude();
    let mut req = request(
        "Use the Bash tool exactly twice, then stop.\n\
         First call: run the command `sleep 8; echo task-finished` with \
         run_in_background set to true.\n\
         Then reply with exactly the word: started\n\
         IMPORTANT: later, when a task notification about that background \
         task arrives, make one FOREGROUND Bash call: `sleep 20; echo waited` \
         (no run_in_background), then reply with exactly: done waiting",
    );
    req.model = None;
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let stream = harness.run(req, controls).await.expect("run starts");

    let collected = tokio::time::timeout(Duration::from_secs(240), async move {
        let mut stream = stream;
        let mut events: Vec<(std::time::Instant, AgentEvent)> = Vec::new();
        let mut dones_seen = 0usize;
        let mut steer = Some(steer_tx);
        let mut steer_sent_at: Option<std::time::Instant> = None;
        let mut steer_task: Option<tokio::task::JoinHandle<std::time::Instant>> = None;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let now = std::time::Instant::now();
            if matches!(ev, AgentEvent::Done { .. }) {
                dones_seen += 1;
                if dones_seen == 1 {
                    // Steer 16s after the first turn settles: the background
                    // task (8s) has exited and the CLI is inside its
                    // self-continued turn's 20s foreground command.
                    let tx = steer.take().expect("one steer");
                    steer_task = Some(tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(16)).await;
                        let at = std::time::Instant::now();
                        let _ = tx
                            .send(SteerMessage {
                                prompt: "what about now".into(),
                                message_id: None,
                            })
                            .await;
                        at
                    }));
                }
            }
            events.push((now, ev));
            if dones_seen == 2 {
                if let Some(task) = steer_task.take() {
                    steer_sent_at = task.await.ok();
                }
                break;
            }
        }
        (events, steer_sent_at)
    })
    .await
    .expect("run should settle without any watchdog — before the fix this timed out");

    let (events, steer_sent_at) = collected;
    let evs: Vec<&AgentEvent> = events.iter().map(|(_, e)| e).collect();
    let done_times: Vec<std::time::Instant> = events
        .iter()
        .filter(|(_, e)| matches!(e, AgentEvent::Done { .. }))
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(done_times.len(), 2, "{evs:?}");
    for (_, e) in events.iter() {
        if let AgentEvent::Done { status, .. } = e {
            assert_eq!(*status, DoneStatus::Completed, "{evs:?}");
        }
    }
    // Prevention path: the steer landed mid self-continued turn, was
    // preceded by a cancel, and its fresh prompt settled promptly — well
    // under the quiet/watchdog windows it used to need.
    let steer_sent_at = steer_sent_at.expect("steer timer ran");
    let steer_turn = done_times[1].duration_since(steer_sent_at);
    assert!(
        steer_turn < Duration::from_secs(25),
        "steer took {steer_turn:?} to settle — prevention should cancel the \
         unowned turn and prompt fresh, not starve into a settle window"
    );
    // And the settle tracked the agent's actual finish (last streamed text),
    // not a watchdog window: cost-frame grace is 1s, allow scheduling slack.
    let last_text_at = events
        .iter()
        .filter(|(_, e)| matches!(e, AgentEvent::TextDelta { .. }))
        .map(|(t, _)| *t)
        .next_back()
        .expect("streamed text exists");
    let settle_gap = done_times[1].duration_since(last_text_at);
    assert!(
        settle_gap < Duration::from_secs(5),
        "final Done lagged the last content by {settle_gap:?} — the settle \
         should ride the turn-end cost frame (~1s)"
    );
}

/// Prevention for the interactive starve: a steer arriving while the agent
/// is mid SELF-CONTINUED turn (open tool call, no prompt outstanding) must
/// not become a session/prompt — the adapter drops that reply. The harness
/// cancels the unowned turn first (the fixture asserts session/cancel is on
/// the wire before any prompt), then dispatches the steer as a fresh prompt
/// after the flush window.
#[tokio::test]
async fn steer_into_self_continuation_cancels_before_prompting() {
    let (controls, steer, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:busy-steer"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            // The self-continued tool call is the busy signal: steer now.
            if matches!(&ev, AgentEvent::ToolCall { id, .. } if id == "sc-1")
                && let Some(tx) = steer.take()
            {
                tx.send(SteerMessage {
                    prompt: "what about now".into(),
                    message_id: None,
                })
                .await
                .expect("steer sent");
                // Sender dropped here; the mailbox closes so the run can end.
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    // Two clean turns: the first prompt's, then the promoted steer's —
    // and the fixture exits with `refusal` if a prompt ever arrives
    // without the preceding session/cancel.
    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fresh answer".into()
    }));
    let steered = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("promoted steer must carry a boundary");
    let first_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("dones asserted above");
    assert!(first_done < steered, "{events:?}");
}

/// Claude's native busy-steer path: a steer into a self-continued turn goes
/// out as a PLAIN prompt (the fixture hard-fails on any session/cancel —
/// cancelling would kill the agent's in-flight work). The CLI folds the
/// message into the running turn natively; the adapter drops the prompt's
/// reply; the cost-frame settle closes the turn ~1s after the merged turn
/// really ends — well before the fixture's held-open stream EOF.
#[tokio::test]
async fn claude_busy_steer_rides_native_queueing_and_the_cost_frame() {
    let (controls, steer, _token) = controls();
    let harness = AcpHarness::claude().with_executable(fixture_path());
    let started = std::time::Instant::now();
    let stream = harness
        .run(request("scenario:native-busy-steer"), controls)
        .await
        .expect("run starts");
    let (events, done2_at) = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut done2_at = None;
        let mut dones_seen = 0usize;
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::ToolCall { id, .. } if id == "sc-2")
                && let Some(tx) = steer.take()
            {
                tx.send(SteerMessage {
                    prompt: "what about now".into(),
                    message_id: None,
                })
                .await
                .expect("steer sent");
            }
            if matches!(ev, AgentEvent::Done { .. }) {
                dones_seen += 1;
                if dones_seen == 2 {
                    done2_at = Some(started.elapsed());
                }
            }
            events.push(ev);
        }
        (events, done2_at)
    })
    .await
    .expect("run finished in time");

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None), (DoneStatus::Completed, None)],
        "{events:?}"
    );
    // The merged turn's folded output streamed through (nothing cancelled)…
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "merged reply".into()
    }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("CANCELLED"))),
        "the native path must never cancel self-continued work: {events:?}"
    );
    // …and the settle rode the cost frame, not the 6s stream EOF.
    let done2_at = done2_at.expect("second done asserted above");
    assert!(
        done2_at < Duration::from_secs(5),
        "settle at {done2_at:?} — should ride the cost frame (~1s), not EOF"
    );
}

/// Every installed real agent, through the one shared loop all the
/// starve/settle changes live in: a short live turn with a mid-turn steer —
/// injection on StepBoundary agents, boundary delivery on TurnBoundary ones,
/// busy-path handling where it applies. Contract per agent that starts:
/// every Done is Completed and the stream ENDS (no stranding) inside the
/// budget. Agents that fail auth/startup are reported and skipped. Run:
/// `cargo test -p zeron-harness --test acp -- --ignored --nocapture real_all_harnesses`
#[tokio::test]
#[ignore = "runs every installed+authenticated agent CLI; costs a few small prompts"]
async fn real_all_harnesses_settle_with_a_mid_turn_steer() {
    let agents: Vec<(&str, AcpHarness)> = vec![
        ("claude", AcpHarness::claude()),
        ("codex", AcpHarness::codex()),
        ("cursor", AcpHarness::cursor()),
        ("grok", AcpHarness::grok()),
        ("pi", AcpHarness::pi()),
    ];
    let mut failures: Vec<String> = Vec::new();
    for (name, h) in agents {
        let (controls, steer_tx, _token) = controls();
        let mut req = request(
            "Write the numbers 1 2 3 4 5, one per line, then stop. \
             If another instruction arrives, follow it too.",
        );
        req.model = None;
        req.reasoning = None;
        req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let stream = match h.run(req, controls).await {
            Ok(s) => s,
            Err(e) => {
                println!("[{name}] SKIP — did not start: {e}");
                continue;
            }
        };
        let outcome = tokio::time::timeout(Duration::from_secs(120), async move {
            let mut stream = stream;
            let mut events = Vec::new();
            let mut steer = Some(steer_tx);
            while let Some(ev) = stream.next().await {
                let ev = ev.expect("stream event");
                if matches!(ev, AgentEvent::TextDelta { .. })
                    && let Some(tx) = steer.take()
                {
                    let _ = tx
                        .send(SteerMessage {
                            prompt: "Also write the word EXTRA on its own line.".into(),
                            message_id: None,
                        })
                        .await;
                    // Sender drops: the mailbox closes, so once every turn
                    // settles the stream must end — stranding shows as the
                    // 120s timeout.
                }
                events.push(ev);
            }
            events
        })
        .await;
        match outcome {
            Err(_) => failures.push(format!("[{name}] STRANDED: stream still open after 120s")),
            Ok(events) => {
                let ds = dones(&events);
                let auth_failure = ds.iter().any(|(s, e)| {
                    *s == DoneStatus::Errored
                        && e.as_deref().is_some_and(|e| {
                            let e = e.to_lowercase();
                            e.contains("auth") || e.contains("login") || e.contains("not installed")
                        })
                });
                if auth_failure {
                    println!("[{name}] SKIP — needs auth: {ds:?}");
                } else if ds.is_empty() || ds.iter().any(|(s, _)| *s != DoneStatus::Completed) {
                    failures.push(format!("[{name}] BAD DONES: {ds:?}"));
                } else {
                    let texts = events
                        .iter()
                        .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
                        .count();
                    println!(
                        "[{name}] OK — {} turn(s) settled, {texts} text deltas",
                        ds.len()
                    );
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Debug variant of the multi-harness sweep, claude only, printing every
/// event with a timestamp — for diagnosing strands the sweep can only name.
/// `cargo test -p zeron-harness --test acp -- --ignored --nocapture real_claude_debug`
#[tokio::test]
#[ignore = "debug harness; needs the claude CLI; costs one small prompt"]
async fn real_claude_debug_steer_trace() {
    let (controls, steer_tx, _token) = controls();
    let h = AcpHarness::claude();
    let mut req = request(
        "Write the numbers 1 2 3 4 5, one per line, then stop. \
         If another instruction arrives, follow it too.",
    );
    req.model = None;
    req.reasoning = None;
    req.cwd = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let started = std::time::Instant::now();
    let stream = h.run(req, controls).await.expect("run starts");
    let _ = tokio::time::timeout(Duration::from_secs(60), async move {
        let mut stream = stream;
        let mut steer = Some(steer_tx);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let t = started.elapsed();
            match &ev {
                AgentEvent::TextDelta { text } => {
                    println!("{t:?} TEXT {:?}", &text[..text.len().min(40)])
                }
                other => println!("{t:?} {other:?}"),
            }
            if matches!(ev, AgentEvent::TextDelta { .. })
                && let Some(tx) = steer.take()
            {
                println!("{:?} >>> sending steer", started.elapsed());
                let _ = tx
                    .send(SteerMessage {
                        prompt: "Also write the word EXTRA on its own line.".into(),
                        message_id: None,
                    })
                    .await;
            }
        }
        println!("{:?} <<< stream ended", started.elapsed());
    })
    .await;
    println!(
        "{:?} === test done (timeout means strand)",
        started.elapsed()
    );
}

/// The injection cost frame must never settle a steered turn: the fixture
/// stamps a mid-turn cost frame right after the injection (real adapter
/// behavior, indistinguishable in shape from the terminal frame), holds the
/// turn open past the 1s cost grace + 2s slack, then finishes normally.
/// Exactly one Done; the post-injection text folds into the same turn.
#[tokio::test]
async fn injection_cost_frame_never_settles_a_steered_turn() {
    let (controls, steer, _token) = controls();
    let harness = AcpHarness::claude().with_executable(fixture_path());
    let stream = harness
        .run(request("scenario:steer-cost-noise"), controls)
        .await
        .expect("run starts");
    let events = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut events = Vec::new();
        let mut stream = stream;
        let mut steer = Some(steer);
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::TextDelta { ref text } if text == "first")
                && let Some(tx) = steer.take()
            {
                tx.send(SteerMessage {
                    prompt: "redirect please".into(),
                    message_id: None,
                })
                .await
                .expect("steer sent");
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("run finished in time");

    assert_eq!(
        dones(&events),
        vec![(DoneStatus::Completed, None)],
        "a premature cost-frame settle would double-Done: {events:?}"
    );
    // The post-injection text arrives BEFORE the single Done — a false
    // settle would flip that order.
    let tail = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "steered tail"))
        .expect("steered tail must fold into the live turn: {events:?}");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done asserted above");
    assert!(tail < done, "{events:?}");
}

#[tokio::test]
async fn autonomous_turn_ended_extension_settles_between_prompts() {
    // A background-task wake makes the agent stream a turn no prompt started;
    // its SDK-side turn-end has no `session/prompt` to settle, so the adapter
    // forwards the `_session/turn_ended` extension instead — which must map
    // to a completed Done exactly once, only for this session, and only
    // between prompts (the engine's quiesce watchdog then stays a backstop,
    // not the settle path).
    let (controls, _steer, _token) = controls();
    let events = run_to_end(&harness(), request("scenario:autonomous-end"), controls).await;

    let d = dones(&events);
    assert_eq!(
        d.len(),
        2,
        "prompt turn + autonomous turn, no third: {events:?}"
    );
    assert!(
        d.iter()
            .all(|(s, e)| *s == DoneStatus::Completed && e.is_none()),
        "{events:?}"
    );

    // The self-continued output sits BETWEEN the two Dones.
    let first_done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .unwrap();
    let last_done = events
        .iter()
        .rposition(|e| matches!(e, AgentEvent::Done { .. }))
        .unwrap();
    let background = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "background finished"))
        .expect("self-continued output surfaces: {events:?}");
    assert!(
        first_done < background && background < last_done,
        "{events:?}"
    );
}
