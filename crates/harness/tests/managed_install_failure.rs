//! The zeronsh/comet#95 condition, reproduced: npm dying silently with an
//! errno-encoded exit (254 = ENOENT — npm/cli#4838) during the adapter
//! fallback. The old `npx -y` path surfaced this as "harness protocol error:
//! initialize: app-server exited before responding; Codex exited unexpectedly
//! (exit code 254)" with nothing actionable. The managed install must instead
//! fail the run with the decoded errno and a recovery hint.
//!
//! Single-test binary: it mutates PATH/SHELL/ZERON_* env process-wide.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeron_harness::{AcpHarness, Harness, HarnessError, RunControls};
use zeron_proto::{RunRequest, SandboxLevel};

#[tokio::test]
async fn silent_npm_enoent_death_surfaces_decoded_error() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // npm as issue #95 saw it: dies with 254 and says nothing.
    let npm = bin.join("npm");
    std::fs::write(&npm, "#!/bin/sh\nexit 254\n").unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();

    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("ZERON_ADAPTERS_DIR", dir.path().join("adapters"));
        std::env::set_var("ZERON_NO_LOGIN_SHELL", "1");
        std::env::set_var("PATH", &bin);
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("CODEX_ACP_EXECUTABLE");
    }

    let harness = AcpHarness::codex();
    let (_steer_tx, steering) = mpsc::channel(1);
    let controls = RunControls {
        request_input: Box::new(|_| tokio::sync::oneshot::channel().1),
        steering,
        interrupt: CancellationToken::new(),
    };
    let request = RunRequest {
        prompt: "hi".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    };

    let err = match harness.run(request, controls).await {
        Err(e) => e,
        Ok(_) => panic!("run() must fail when the adapter install fails"),
    };
    let HarnessError::Install(message) = &err else {
        panic!("expected an Install error, got: {err}");
    };
    assert!(message.contains("exit code 254"), "{message}");
    assert!(message.contains("ENOENT"), "{message}");
    assert!(message.contains("failed silently"), "{message}");
    assert!(
        message.contains("codex-acp"),
        "names the package: {message}"
    );
}
