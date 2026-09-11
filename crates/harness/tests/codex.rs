//! CodexHarness integration tests against the fake app server in
//! `tests/fixtures/fake-codex.sh` (no real `codex` binary involved).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{
    CancellationToken, CodexHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, TodoItem,
    ToolCall, UserInputAnswer, UserInputQuestion,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-codex.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> CodexHarness {
    CodexHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: Some("gpt-5.6-sol".into()),
        reasoning: Some(ReasoningLevel::Ultra),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
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
    harness: &CodexHarness,
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

#[tokio::test]
async fn reasoning_preserves_summary_parts_and_item_boundaries_per_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:reasoning"), controls).await;
    let mut parent = String::new();
    let mut child = String::new();
    for event in &events {
        match event {
            AgentEvent::ReasoningDelta { text } => parent.push_str(text),
            AgentEvent::Subagent { event, .. } => {
                if let AgentEvent::ReasoningDelta { text } = event.as_ref() {
                    child.push_str(text);
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        parent,
        "**Implementing file badges**\n\n**Preparing fixture screenshots**\n\nChecking the final result."
    );
    assert_eq!(child, "**Checking layout**\n\nInspecting the output panel.");
}

#[tokio::test]
async fn happy_path_maps_deltas_items_usage_and_done() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    req.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), req, controls).await;

    // SessionStarted from thread/start's thread id.
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
    assert_eq!(*h, HarnessId::Codex);
    assert_eq!(model, "gpt-5.6-sol");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "th-1");

    // Deltas — both wire spellings accepted.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking hard".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "summary".into()
    }));

    // commandExecution: ToolCall at started only, exit code 1 => error result.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"))
            .count(),
        1
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "c1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "c1".into(),
        is_error: true,
        output: None,
        diff: None,
    }));

    // fileChange (single add): WriteFile, refreshed at completion.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    id,
                    call: ToolCall::WriteFile { path, content: None }
                } if id == "f1" && path == "/tmp/new.rs"
            ))
            .count(),
        2,
        "started + completion-refresh: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "f1".into(),
        is_error: false,
        output: None,
        diff: None,
    }));

    // mcpToolCall with failed status.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "mcp1".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "mcp1".into(),
        is_error: true,
        output: None,
        diff: None,
    }));

    // webSearch lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "w1".into(),
        call: ToolCall::WebSearch {
            query: "rust".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "w1".into(),
        is_error: false,
        output: None,
        diff: None,
    }));

    // Completion-only todoList still opens and closes the lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "td1".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "a".into(),
                    done: true
                },
                TodoItem {
                    text: "b".into(),
                    done: false
                },
            ]
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "td1".into(),
        is_error: false,
        output: None,
        diff: None,
    }));

    // Streamed agentMessage must not re-emit its completed text…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Hello world")),
        "streamed message text re-emitted: {events:?}"
    );
    // …but a never-streamed one falls back to the completed text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "unstreamed tail".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
            .count(),
        2
    );

    // Usage rides just before the terminal Done.
    let usage_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Usage {
                    input_tokens: 42,
                    output_tokens: 7
                }
            )
        })
        .expect("usage emitted");
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done emitted");
    assert!(usage_pos < done_pos);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn steering_uses_turn_steer_with_expected_turn_id() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer"), controls).await;

    let steered = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Steered {
                assistant_message_id,
                next_assistant_message_id,
            } => Some((
                assistant_message_id.clone(),
                next_assistant_message_id.clone(),
            )),
            _ => None,
        })
        .expect("Steered emitted: {events:?}");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake only emits this delta after verifying expectedTurnId + text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn rejected_steer_falls_back_to_a_follow_up_turn() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer-race"), controls).await;

    // Two turns: the raced one completes, then the fallback carries the steer.
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        dones,
        vec![DoneStatus::Completed, DoneStatus::Completed],
        "{events:?}"
    );
    let steered_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("Steered emitted on fallback");
    let first_done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("first done");
    assert!(
        first_done_pos < steered_pos,
        "fallback turn starts after the raced turn ends: {events:?}"
    );
    // Only emitted by the fake when the fallback turn/start carried the text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fallback".into()
    }));
}

#[tokio::test]
async fn approvals_round_trip_as_input_requests() {
    // Approvals must reach the ENGINE's input bridge (`request_input`) — and
    // the harness must NOT emit its own `InputRequested`/`InputResolved`
    // twins: the bridge owns that lifecycle (it mints the request id the
    // resolver is parked under; a harness-emitted copy folded an unanswerable
    // duplicate chip into the doc).
    let asked: Arc<Mutex<Vec<UserInputQuestion>>> = Arc::new(Mutex::new(Vec::new()));
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let _steer = steer_tx;
    let token = CancellationToken::new();
    let seen = asked.clone();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            seen.lock().unwrap().extend(questions.iter().cloned());
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
    let mut req = request("scenario:approve");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{events:?}");
    assert_eq!(asked[0].header, "Approve command");
    assert!(asked[0].question.contains("rm -rf /tmp/x"));
    assert_eq!(asked[0].options, vec!["Yes".to_string(), "No".to_string()]);
    assert_eq!(asked[1].header, "Approve file change");
    assert!(asked[1].question.contains("/tmp/a.rs"));
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // The fake only completes the turn after seeing BOTH accept decisions.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn approval_no_answer_becomes_decline() {
    let (controls, _steer, _token) = controls("No");
    let mut req = request("scenario:decline");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    // The fake only completes the turn after seeing the decline decision.
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
async fn interrupt_sends_turn_interrupt_and_maps_aborted() {
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel(); // interrupt mid-turn
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("interrupt completed in time");

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn unresponsive_child_is_reaped_with_interrupted_done() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");

    let events = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
        events
    })
    .await
    .expect("escalation completed in time");

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn turn_failed_maps_to_errored_done() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:fail"), controls).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("boom".into()),
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn resume_falls_back_to_fresh_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-fail".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-fresh"
        )),
        "fresh thread expected: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-fresh".into()),
        })
    );
}

#[tokio::test]
async fn resume_reuses_the_existing_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-ok".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-resumed"
        )),
        "resumed thread expected: {events:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    let (controls, _steer, _token) = controls("Yes");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

#[tokio::test]
async fn models_discovers_visible_catalog_with_pagination() {
    let models = harness().models().await.expect("models");
    assert_eq!(models.len(), 3);
    assert_eq!(models[0].id, "gpt-6-astra");
    assert_eq!(models[1].id, "gpt-5.6-terra");
    assert_eq!(models[2].id, "gpt-5.6-sol");
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
    assert!(models.iter().any(|m| m.id == "gpt-5.6-sol"));
    let tier = models[0]
        .options
        .iter()
        .find(|option| option.id == "serviceTier")
        .expect("Astra service tier");
    assert_eq!(tier.choices[0].id, "default");
    assert_eq!(tier.choices[1].id, "fast");
    assert_eq!(tier.choices.len(), 2, "priority and fast dedupe");

    // A failed probe stays useful and includes the new model in the fallback.
    let fallback = CodexHarness::new()
        .with_executable("/bin/false")
        .models()
        .await
        .expect("fallback models");
    assert_eq!(fallback.len(), 9);
    assert_eq!(fallback[0].id, "gpt-6-astra");

    let missing = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    // models() requires a resolvable binary… but with_executable trusts the
    // caller's path, so only the default resolution can report NotInstalled —
    // exercise the harness identity surface instead.
    assert_eq!(missing.id(), HarnessId::Codex);
    // "Codex" — comet composer/defaults.ts HARNESS_LABEL (and the registry's
    // lazy descriptor must stay in lockstep).
    assert_eq!(missing.display_name(), "Codex");
    assert_eq!(missing.reasoning_levels().len(), 7);
}

#[tokio::test]
async fn resumed_parent_recovers_v1_and_v2_child_owners_without_replaying_chips() {
    for mode in ["v1", "v2"] {
        let mut req = request("scenario:resumed-child");
        req.resume = Some(format!("resume-with-child-{mode}"));
        let (controls, _steer, _token) = controls("Yes");
        let events = run_to_end(&harness(), req, controls).await;
        assert!(
            !events.iter().any(
                |e| matches!(e, AgentEvent::ToolCall { call, .. } if call.is_subagent_spawn())
            )
        );
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::Subagent { parent_tool_use_id, event }
            if parent_tool_use_id == "spawn-alpha" && matches!(event.as_ref(), AgentEvent::TextDelta { text } if text == "resumed alpha")
        )), "{mode}: {events:?}");
    }
}

#[tokio::test]
async fn v2_lifecycle_reuses_chips_and_reopens_the_same_child_for_followup() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:v2-lifecycle"), controls).await;
    let spawns: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, call } if call.is_subagent_spawn() => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(spawns, ["spawn-alpha", "spawn-beta"]);
    let mut alpha_text = String::new();
    let mut alpha_users = Vec::new();
    let mut alpha_done = Vec::new();
    for e in &events {
        if let AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } = e
        {
            assert!(spawns.contains(&parent_tool_use_id.as_str()));
            if parent_tool_use_id == "spawn-alpha" {
                match event.as_ref() {
                    AgentEvent::TextDelta { text } => alpha_text.push_str(text),
                    AgentEvent::UserMessage { text } => alpha_users.push(text.as_str()),
                    AgentEvent::Done { status, error, .. } => {
                        alpha_done.push((*status, error.as_deref()))
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(alpha_text, "first alpha\n\nsecond alpha\n\n");
    assert_eq!(alpha_users, ["First assignment"]);
    assert_eq!(
        alpha_done,
        [
            (DoneStatus::Completed, None),
            (DoneStatus::Errored, Some("followup failed"))
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Done { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn v1_spawns_bind_children_and_controls_do_not_create_agents() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:v1-subagents"), controls).await;
    let spawns: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, call } if call.is_subagent_spawn() => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        spawns,
        std::collections::HashSet::from(["spawn-alpha", "spawn-beta"])
    );
    for e in &events {
        if let AgentEvent::Subagent {
            parent_tool_use_id, ..
        } = e
        {
            assert!(spawns.contains(parent_tool_use_id.as_str()), "{e:?}");
        }
    }
    for (owner, text) in [
        ("spawn-alpha", "alpha answer"),
        ("spawn-beta", "beta answer"),
    ] {
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::Subagent { parent_tool_use_id, event }
            if parent_tool_use_id == owner && matches!(event.as_ref(), AgentEvent::TextDelta { text: t } if t == text)
        )));
    }
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::Subagent { parent_tool_use_id, event }
        if parent_tool_use_id == "spawn-beta" && matches!(event.as_ref(), AgentEvent::ToolCall { id, .. } if id == "beta-tool")
    )));
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::Subagent { parent_tool_use_id, event }
        if parent_tool_use_id == "spawn-beta" && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "Also check gamma")
    )));
    assert_eq!(events.iter().filter(|e| matches!(e, AgentEvent::Subagent { event, .. } if matches!(event.as_ref(), AgentEvent::Done { .. }))).count(), 2);
    let parent: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(parent, "parent answer");
}

#[tokio::test]
async fn child_identity_survives_early_output_and_later_activity_ids() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:child-identity"), controls).await;
    let spawn = events
        .iter()
        .position(|e| {
            matches!(e,
                AgentEvent::ToolCall { id, .. } if id == "spawn-alpha"
            )
        })
        .unwrap();
    let early = events.iter().position(|e| matches!(e,
        AgentEvent::Subagent { parent_tool_use_id, event }
        if parent_tool_use_id == "spawn-alpha"
            && matches!(event.as_ref(), AgentEvent::TextDelta { text } if text == "early alpha")
    )).unwrap();
    assert!(
        spawn < early,
        "the chip must exist before buffered traffic binds"
    );
    let mut alpha = String::new();
    let mut beta = String::new();
    for e in &events {
        if let AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } = e
        {
            assert!(matches!(
                parent_tool_use_id.as_str(),
                "spawn-alpha" | "spawn-beta"
            ));
            if let AgentEvent::TextDelta { text } = event.as_ref() {
                if parent_tool_use_id == "spawn-alpha" {
                    alpha.push_str(text);
                } else {
                    beta.push_str(text);
                }
            }
        }
    }
    assert_eq!(alpha, "early alphalater alpha");
    assert_eq!(beta, "beta outputbeta continues");
    let parent: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(parent, "parent output");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Done { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn child_thread_routing_tags_and_never_settles_parent() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:subagent"), controls).await;

    // Exactly one Done — the child's turn/completed must NOT settle the
    // parent turn (the swallowed-catch-all bug class this table exists for).
    let dones: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, AgentEvent::Done { .. }).then_some(i))
        .collect();
    assert_eq!(dones.len(), 1, "one parent Done only: {events:?}");

    // Parent output that follows the child's turn/completed still streams.
    let late_parent = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "parent still going"))
        .expect("parent delta after child turn end");
    assert!(late_parent < dones[0]);

    // The spawn chip lives on the parent feed, named from the agent path,
    // and resolves when the activity completes.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCall { id, call: ToolCall::Unknown { name, .. } }
            if id == "call_alpha" && name == "Agent: alpha"
    )));
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { id, is_error: false, .. } if id == "call_alpha")
    ));
    // The root's own subAgentActivity produces no chip.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "call_root")),
        "root self-activity must not register or render: {events:?}"
    );

    // Child deltas and items arrive tagged with the spawn call id — never
    // bare (child threads stream deltas on this wire; live-verified 0.146.1).
    assert!(events.contains(&AgentEvent::Subagent {
        parent_tool_use_id: "call_alpha".into(),
        event: Box::new(AgentEvent::TextDelta {
            text: "child says hi".into()
        }),
    }));
    assert!(events.contains(&AgentEvent::Subagent {
        parent_tool_use_id: "call_alpha".into(),
        event: Box::new(AgentEvent::ToolCall {
            id: "cs1".into(),
            call: ToolCall::Exec {
                command: "echo hi".into()
            },
        }),
    }));
    assert!(events.contains(&AgentEvent::Subagent {
        parent_tool_use_id: "call_alpha".into(),
        event: Box::new(AgentEvent::ToolResult {
            id: "cs1".into(),
            is_error: false,
            output: None,
            diff: None,
        }),
    }));
    // The parent's steer (a userMessage item on the CHILD thread) arrives as
    // exactly one tagged UserMessage — completed only, never doubled by the
    // started lifecycle event, never leaked untagged.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Subagent { parent_tool_use_id, event }
                    if parent_tool_use_id == "call_alpha"
                        && matches!(event.as_ref(), AgentEvent::UserMessage { text } if text == "also check the rebuild")
            ))
            .count(),
        1,
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::UserMessage { .. })),
        "steer leaked into the parent feed: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "cs1")),
        "child tool call leaked into the parent feed: {events:?}"
    );

    // The child's turn/completed (and later thread/closed) become tagged
    // terminal events — real fan-outs never call close_agent, so the turn
    // end is what flips the chip off "running".
    assert!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Subagent { parent_tool_use_id, event }
                    if parent_tool_use_id == "call_alpha"
                        && matches!(event.as_ref(), AgentEvent::Done { status: DoneStatus::Completed, .. })
            ))
            .count()
            >= 1,
        "{events:?}"
    );
    // The tagged terminal must arrive from turn/completed — BEFORE the
    // parent delta that follows it in the script (not only at thread/closed).
    let child_done = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Subagent { parent_tool_use_id, event }
                    if parent_tool_use_id == "call_alpha"
                        && matches!(event.as_ref(), AgentEvent::Done { .. })
            )
        })
        .expect("tagged done");
    let late_parent_delta = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TextDelta { text } if text == "parent still going"))
        .expect("parent delta");
    assert!(child_done < late_parent_delta, "{events:?}");
}

/// Run once per multi-agent mode using a wrapper supplied via
/// CODEX_SUBAGENT_TEST_EXECUTABLE. The wrapper can set feature flags for its
/// process without changing the user's config. This deliberately consumes
/// model calls and is never part of the offline suite.
#[tokio::test]
#[ignore = "real Codex spawn, followup and resume; needs install, auth and network"]
async fn live_subagent_spawn_and_followup_keep_one_transcript() {
    let executable = std::env::var_os("CODEX_SUBAGENT_TEST_EXECUTABLE")
        .expect("set CODEX_SUBAGENT_TEST_EXECUTABLE to a wrapper selecting v1 or v2");
    let harness = CodexHarness::new().with_executable(executable);
    let cwd = tempfile::tempdir().unwrap();
    let mut req = request(
        "This is an integration smoke test. Spawn EXACTLY ONE subagent (name it alpha if naming is supported). Its entire task is: reply exactly child-first. It must not use any tools or spawn agents. Wait for it to finish, then reply exactly parent-first. Do not close the child; we will reuse it. Do not inspect or change any files.",
    );
    req.cwd = cwd.path().display().to_string();
    req.reasoning = Some(ReasoningLevel::Low);
    if let Ok(model) = std::env::var("CODEX_SUBAGENT_TEST_MODEL") {
        req.model = Some(model);
    }
    let (run_controls, mut steer, mut interrupt) = controls("Yes");
    let mut stream = harness
        .run(req.clone(), run_controls)
        .await
        .expect("run starts");
    let mut events = Vec::new();
    for turn in 0..3 {
        if turn == 1 {
            steer.send(SteerMessage {
                prompt: "Reuse the SAME existing subagent for one more task: reply exactly child-second. Use followup_task if available, otherwise send_input. Do not spawn a new agent. Wait for it to finish, then reply exactly parent-second. Do not inspect or change files.".into(),
                message_id: None,
            }).await.unwrap();
        }
        if turn == 2 {
            // End the first app-server process while idle, then resume the
            // parent in a fresh process and address its already-known child.
            interrupt.cancel();
            tokio::time::timeout(Duration::from_secs(10), async {
                while stream.next().await.is_some() {}
            })
            .await
            .expect("old app-server stops");
            req.resume = events.iter().find_map(|e| match e {
                AgentEvent::SessionStarted { session_id, .. } => Some(session_id.clone()),
                _ => None,
            });
            req.prompt = "The app-server has restarted. Reuse the SAME existing subagent again: reply exactly child-third. Use followup_task if available. Otherwise first use resume_agent with the existing child's id to reactivate it, then send_input. Do not spawn another agent. Wait for its actual child-third reply before replying exactly parent-third. If a tool fails, recover using the same child id; never claim the child finished without its reply. Do not inspect or change files.".into();
            let (run_controls, new_steer, new_interrupt) = controls("Yes");
            steer = new_steer;
            interrupt = new_interrupt;
            stream = harness
                .run(req.clone(), run_controls)
                .await
                .expect("parent resumes");
        }
        let result = tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(event) = stream.next().await {
                let event = event.expect("stream event");
                let done = matches!(event, AgentEvent::Done { .. });
                let failed = matches!(
                    event,
                    AgentEvent::Done {
                        status: DoneStatus::Errored | DoneStatus::Interrupted,
                        ..
                    }
                );
                events.push(event);
                assert!(!failed, "parent failed: {:?}", events.last());
                if done {
                    return;
                }
            }
            panic!("stream ended before parent completion");
        })
        .await;
        if result.is_err() {
            interrupt.cancel();
        }
        result.expect("live turn finishes within 120 seconds");
    }
    drop(stream);
    if let Some(path) = std::env::var_os("CODEX_SUBAGENT_TEST_CAPTURE") {
        std::fs::write(path, serde_json::to_vec_pretty(&events).unwrap()).unwrap();
    }
    let spawns: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCall { id, call } if call.is_subagent_spawn() => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(spawns.len(), 1, "one original spawn: {spawns:?}");
    if let Ok(mode) = std::env::var("CODEX_SUBAGENT_TEST_MODE") {
        let expected = match mode.as_str() {
            "v1" => "collabAgentToolCall",
            "v2" => "subAgentActivity",
            _ => panic!("unknown mode {mode}"),
        };
        assert!(
            events.iter().any(|e| matches!(e,
                AgentEvent::ToolCall { call: ToolCall::Unknown { input: Some(input), .. }, .. }
                if input.get("type").and_then(serde_json::Value::as_str) == Some(expected)
            )),
            "the model must actually use {mode}"
        );
    }
    let mut text = String::new();
    let mut terminals = 0;
    for event in &events {
        if let AgentEvent::Subagent {
            parent_tool_use_id,
            event,
        } = event
        {
            assert_eq!(parent_tool_use_id, spawns[0]);
            if let AgentEvent::TextDelta { text: delta } = event.as_ref() {
                text.push_str(delta);
            }
            if matches!(
                event.as_ref(),
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            ) {
                terminals += 1;
            }
        }
    }
    for reply in ["child-first", "child-second", "child-third"] {
        assert_eq!(text.matches(reply).count(), 1, "child transcript: {text:?}");
    }
    assert_eq!(terminals, 3, "one child completion per assignment");
}

/// Live smoke against the REAL codex app-server (installed + authed):
/// one trivial turn, ending on turn/completed.
/// `cargo test -p zeron-harness --test codex -- --ignored`.
#[tokio::test]
#[ignore = "spawns the real codex app-server; needs install + auth + network"]
async fn live_real_app_server_single_turn() {
    let harness = CodexHarness::new();
    let mut req = request("Reply with exactly the word: pong");
    req.cwd = std::env::temp_dir().display().to_string();
    let (controls, _steer, _token) = controls("Yes");
    let mut stream = harness.run(req, controls).await.expect("run starts");
    // The session parks after the turn (steering mailbox open) — collect up
    // to the first Done, not stream end.
    let events = tokio::time::timeout(Duration::from_secs(120), async {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("live turn finished in time");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { .. })),
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

// ---------------------------------------------------------------------------
// Slash-command discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commands_come_from_skills_list() {
    let h = harness();
    let commands = h.commands().await.expect("discovery succeeds");
    assert_eq!(
        commands.len(),
        2,
        "same-name skills across cwd groups dedupe: {commands:?}"
    );
    assert_eq!(commands[0].name, "imagegen");
    assert_eq!(
        commands[0].description, "Generate or edit images",
        "interface.shortDescription wins over the model-facing paragraph"
    );
    assert_eq!(commands[1].name, "bare");
    assert_eq!(
        commands[1].description, "No interface block",
        "top-level description is the fallback"
    );
    assert_eq!(h.commands().await.expect("cache hit"), commands);
}

/// Live smoke against the real CLI: `cargo test -p zeron-harness --test
/// codex -- --ignored live_commands`.
#[tokio::test]
#[ignore]
async fn live_commands_discovery() {
    let h = CodexHarness::new();
    let commands = h.commands().await.expect("live discovery");
    eprintln!("{} commands, first: {:?}", commands.len(), commands.first());
}

#[tokio::test]
async fn title_run_preserves_read_only_and_replaces_coding_instructions() {
    let (controls, _steer, token) = controls("Yes");
    let stream = harness()
        .run_title(request("scenario:title"), controls)
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .unwrap();
    token.cancel();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Fix Login Flow")),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "{events:?}"
    );
}
