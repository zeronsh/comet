//! Real-world E2E for the managed adapter install: with no `pi-acp`
//! binary anywhere, `run()` must npm-install the pinned adapter into
//! `$ZERON_ADAPTERS_DIR`, spawn it via node, and reach SessionStarted (the
//! full initialize → session/new handshake) — the exact path that used to be
//! `npx -y` at chat time (zeronsh/comet#95).
//!
//! Ignored: needs network, npm, and the pi CLI on the machine. Run with
//! `cargo test -p zeron-harness --test managed_install -- --ignored`.
//!
//! Single-test binary: it mutates ZERON_ADAPTERS_DIR process-wide.

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeron_harness::{AcpHarness, Harness, RunControls};
use zeron_proto::{AgentEvent, RunRequest};

#[tokio::test]
#[ignore = "network + npm + codex CLI; installs the pinned adapter for real"]
async fn managed_install_reaches_session_started() {
    let adapters = tempfile::tempdir().unwrap();
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("ZERON_ADAPTERS_DIR", adapters.path());
        std::env::remove_var("PI_ACP_EXECUTABLE");
    }

    let harness = AcpHarness::pi();
    let (_steer_tx, steering) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| tokio::sync::oneshot::channel().1),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = RunRequest {
        prompt: "say the word ok and stop".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: std::env::temp_dir().display().to_string(),
        sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        harness.run(request, controls),
    )
    .await
    .expect("run() (including the npm install) finished in time")
    .expect("run() resolved a launch");

    // The handshake proves the whole chain: managed install → node spawn →
    // initialize → session/new. Interrupt as soon as the session opens so
    // the test never burns a real turn.
    let mut started = false;
    while let Some(ev) = tokio::time::timeout(std::time::Duration::from_secs(180), stream.next())
        .await
        .expect("stream made progress")
    {
        match ev.expect("stream event") {
            AgentEvent::SessionStarted { session_id, .. } => {
                assert!(!session_id.is_empty(), "handshake produced a session id");
                started = true;
                interrupt.cancel();
            }
            AgentEvent::Done { status, error, .. } => {
                assert!(
                    started,
                    "run died before SessionStarted: {status:?} {error:?}"
                );
                break;
            }
            _ => {}
        }
    }
    assert!(started, "stream ended without SessionStarted");

    // The install landed in the managed dir (not the user's npm state) and
    // is marked complete, so the next launch skips npm entirely.
    let root = adapters.path().join("agentclientprotocol__pi-acp");
    let version_dir = std::fs::read_dir(&root)
        .expect("managed install dir exists")
        .flatten()
        .next()
        .expect("a pinned version dir")
        .path();
    assert!(version_dir.join(".zeron-install-ok").exists());
}
