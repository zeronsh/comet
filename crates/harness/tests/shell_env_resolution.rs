//! End-to-end: a CLI visible only through the login shell's PATH resolves.
//!
//! This file must stay a single test: it mutates process env (SHELL/PATH/HOME)
//! and warms the process-global login-shell snapshot cache, so it needs its
//! own test binary with no parallel siblings.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use zeron_harness::{AcpHarness, Harness as _};

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn cli_on_login_shell_path_only_is_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    write_executable(&shell_bin.join("devin"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("hermes"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("pi-acp"), "#!/bin/sh\nexit 0\n");
    write_executable(&shell_bin.join("claude"), "#!/bin/sh\nexit 0\n");

    // A $SHELL whose init shapes PATH and exports provider-style env vars —
    // the shape resolution and env forwarding must both survive.
    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             ZERON_TEST_LOGIN_VAR=from-login-shell; export ZERON_TEST_LOGIN_VAR\n\
             ZERON_TEST_BOTH_VAR=from-login-shell; export ZERON_TEST_BOTH_VAR\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    // A GUI/service-launch environment: minimal PATH, no CLIs reachable, HOME
    // pointed away from any real install dirs. ZERON_TEST_BOTH_VAR is already
    // defined by the daemon (this process): the process value must win.
    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", "/usr/bin:/bin");
        std::env::set_var("ZERON_TEST_BOTH_VAR", "from-process");
        std::env::remove_var("DEVIN_EXECUTABLE");
        std::env::remove_var("HERMES_EXECUTABLE");
        std::env::remove_var("PI_ACP_EXECUTABLE");
        std::env::remove_var("CLAUDE_CODE_EXECUTABLE");
        std::env::remove_var("ZERON_NO_LOGIN_SHELL");
        std::env::remove_var("ZERON_NO_LOGIN_SHELL_ENV");
    }

    let snapshot = zeron_harness::shell_env::login_shell_path().expect("snapshot captured");
    let snapshot = snapshot.to_string_lossy();
    assert!(
        snapshot.starts_with(&format!("{}:", shell_bin.display())),
        "snapshot should carry the shell-shaped PATH, got: {snapshot}"
    );

    // Exported rc-file variables are forwarded to spawned children — except
    // those the daemon already defines, and the probe's own markers.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("exit 0");
    zeron_harness::shell_env::apply_login_shell_env(&mut cmd);
    let child_env: std::collections::HashMap<_, _> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
        .collect();
    assert_eq!(
        child_env
            .get(std::ffi::OsStr::new("ZERON_TEST_LOGIN_VAR"))
            .map(|v| v.to_string_lossy().to_string())
            .as_deref(),
        Some("from-login-shell"),
        "login-shell export must reach the child"
    );
    // ZERON_TEST_BOTH_VAR already exists in the daemon's env, so the snapshot
    // must not re-set it on the command: the child inherits the process value.
    assert!(
        child_env
            .get(std::ffi::OsStr::new("ZERON_TEST_BOTH_VAR"))
            .is_none_or(|v| v == "from-process"),
        "an existing process env var must never be overridden"
    );
    assert!(
        !child_env.contains_key(std::ffi::OsStr::new("ZERON_RESOLVING_ENVIRONMENT")),
        "the probe marker must never reach real children"
    );

    // The agent binaries are only reachable through the snapshot; the
    // launch program (not the npx fallback) must be the shell-PATH binary,
    // proving resolution consulted the login-shell snapshot.
    // Native drivers consult the same snapshot for the agent CLI itself.
    assert!(
        zeron_harness::ClaudeHarness::new().installed(),
        "claude resolves via login-shell PATH"
    );
    let devin = AcpHarness::devin()
        .launch_program()
        .expect("devin resolves via login-shell PATH");
    assert_eq!(devin, shell_bin.join("devin"), "{devin:?}");
    let hermes = AcpHarness::hermes()
        .launch_program()
        .expect("hermes resolves via login-shell PATH");
    assert_eq!(hermes, shell_bin.join("hermes"), "{hermes:?}");
    let pi = AcpHarness::pi()
        .launch_program()
        .expect("pi-acp resolves via login-shell PATH");
    assert_eq!(pi, shell_bin.join("pi-acp"), "{pi:?}");
}
