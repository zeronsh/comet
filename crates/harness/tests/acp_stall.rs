//! Prompt-stall watchdog (`ZERON_ACP_PROMPT_STALL_MS`): a grok-spec agent
//! that goes TOTALLY silent after the prompt — the wedged-shared-leader
//! signature from the field — must surface a visible error and an errored
//! Done instead of indefinite Working. Own test binary: the env knob is
//! process-global.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

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

#[tokio::test]
async fn silent_agent_errors_via_the_prompt_stall_watchdog() {
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("ZERON_ACP_PROMPT_STALL_MS", "700");
    }
    let (_steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let request = RunRequest {
        prompt: "scenario:prompt-stall".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let harness = AcpHarness::grok().with_executable(fixture_path());
    let stream = harness.run(request, controls).await.expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("watchdog closed the run in time");

    let error = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("visible error chip");
    assert!(
        error.contains("did not respond"),
        "error names the stall: {error}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(_),
            ..
        })
    ));
}

/// Same watchdog, opencode spec: a failing model provider is INVISIBLE on
/// opencode's wire (the CLI retries the provider stream forever without
/// failing `session/prompt` — verified live, 1.18.18), so the stall chip is
/// the only feedback a user with a broken/unauthorized provider ever gets.
/// The hint must point at the provider, not at a wedged process.
#[tokio::test]
async fn opencode_stall_hint_names_the_provider() {
    unsafe {
        std::env::set_var("ZERON_ACP_PROMPT_STALL_MS", "700");
    }
    let (_steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    let request = RunRequest {
        prompt: "scenario:prompt-stall".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    let harness = AcpHarness::opencode().with_executable(fixture_path());
    let stream = harness.run(request, controls).await.expect("run starts");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("watchdog closed the run in time");

    let error = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("visible error chip");
    assert!(
        error.contains("provider") && error.contains("opencode auth"),
        "hint points at the provider setup: {error}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(_),
            ..
        })
    ));
}
