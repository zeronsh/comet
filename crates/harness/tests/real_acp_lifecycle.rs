//! Real Pi ACP + model regression for #296. Requires an authenticated Pi and
//! fixtures/pi-slow-model.ts loaded as a Pi extension (35s delay by default).
//! PI_ACP_EXECUTABLE and PI_ACP_PI_COMMAND can select isolated installations.
//! cargo test -p zeron-harness --test real_acp_lifecycle -- --ignored --nocapture

use futures::StreamExt;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};

async fn live_run(cancel: bool) {
    let cwd = tempfile::tempdir().unwrap();
    let (steer, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        steering,
        interrupt: interrupt.clone(),
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
    };
    let request = RunRequest {
        prompt: "Run the shell command `printf ACP-TOOL-OK` exactly once using bash. After seeing its result, reply exactly FIRST-DONE. Do not call any other tools.".into(),
        harness: None, model: None, reasoning: None,
        model_options: serde_json::Map::new(), cwd: cwd.path().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite, auto_approve: true,
        attachments: Vec::new(), worktree: None, resume: None,
    };
    let mut stream = AcpHarness::pi()
        .run(request, controls)
        .await
        .expect("real Pi must start");
    let mut tool_at = None;
    let mut done_count = 0;
    let mut text = String::new();
    let mut cancel_sent = false;
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(240), async {
        loop {
            let event = tokio::select! {
                event = stream.next() => event.expect("stream ended before all turns settled").expect("event"),
                _ = tokio::time::sleep_until(tool_at.unwrap_or_else(tokio::time::Instant::now) + Duration::from_secs(32)),
                    if cancel && tool_at.is_some() && !cancel_sent => {
                        assert_eq!(done_count, 0, "quiet turn must still be active");
                        cancel_sent = true;
                        interrupt.cancel();
                        continue;
                    }
            };
            match event {
                AgentEvent::ToolResult { .. } if tool_at.is_none() => {
                    tool_at = Some(tokio::time::Instant::now());
                    // Both messages must queue until the slow original prompt
                    // responds. The real adapter rejects overlapping prompts.
                    for word in ["SECOND-DONE", "THIRD-DONE"] {
                        steer.send(SteerMessage { prompt: format!("Do not call tools. Reply exactly {word}."), message_id: None }).await.unwrap();
                    }
                }
                AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                AgentEvent::Done { status, error, .. } => {
                    done_count += 1;
                    if cancel {
                        assert!(cancel_sent, "premature completion: {status:?} {error:?}");
                        assert_eq!(status, DoneStatus::Interrupted, "{error:?}");
                        break;
                    }
                    assert_eq!(status, DoneStatus::Completed, "{error:?}");
                    let expected = ["FIRST-DONE", "SECOND-DONE", "THIRD-DONE"][done_count - 1];
                    assert!(text.contains(expected), "turn {done_count}: {text:?}");
                    if done_count == 1 {
                        let gap = tool_at.expect("model must call a tool").elapsed();
                        assert!(gap >= Duration::from_secs(35), "delay extension did not run: {gap:?}");
                        eprintln!("post-tool gap {gap:?}; original prompt settled correctly");
                    }
                    text.clear();
                    if done_count == 3 { break; }
                }
                _ => {}
            }
        }
    }).await;
    if outcome.is_err() {
        interrupt.cancel();
    }
    outcome.expect("live lifecycle test timed out");
    drop(steer);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
            .is_none(),
        "unexpected events after final turn"
    );
    eprintln!(
        "real Pi lifecycle: cancel={cancel}, dones={done_count}, elapsed={:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "calls an authenticated real model; requires the slow-model Pi extension"]
async fn real_pi_slow_model_preserves_prompt_and_queued_followups() {
    let runs = std::env::var("ACP_TEST_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    for _ in 0..runs {
        live_run(false).await;
    }
}

#[tokio::test]
#[ignore = "calls an authenticated real model; requires the slow-model Pi extension"]
async fn real_pi_cancel_during_post_tool_silence() {
    live_run(true).await;
}
