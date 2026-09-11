//! The codex "Add account" flow must launch the same executable the harness
//! runs — `CODEX_EXECUTABLE`, PATH discovery, and the npm `.cmd` shim layout
//! included — instead of a bare `Command::new("codex")` that only finds a
//! top-level `codex.exe`.
//!
//! Child-process probes: resolution reads process env (PATH, the override),
//! so each case re-runs this test binary with a controlled environment.

use std::path::{Path, PathBuf};

use zeron_engine::{AgentAccounts, AgentAccountsConfig};
use zeron_proto::{AgentLoginMode, HarnessId};

fn test_accounts(root: &Path) -> AgentAccounts {
    let config = AgentAccountsConfig {
        data_dir: root.join("data"),
        claude_config_dir: root.join("claude"),
        claude_config_file: root.join("claude.json"),
        codex_home: root.join("codex"),
        cursor_sdk_auth_file: root.join("cursor-sdk").join("auth.json"),
    };
    AgentAccounts::new(config)
}

const FAKE_URL: &str = "https://auth.openai.com/authorize?probe=fake";

/// The fake CLI prints the authorize URL and then parks so the login flow
/// stays alive until the probe cancels it.
fn fake_codex(dir: &Path) -> PathBuf {
    let path = dir.join(if cfg!(windows) {
        "fake-codex.cmd"
    } else {
        "fake-codex"
    });
    if cfg!(windows) {
        std::fs::write(
            &path,
            format!(
                "@echo off\r\necho open {FAKE_URL} in your browser\r\nping -n 30 127.0.0.1 >nul\r\n"
            ),
        )
        .expect("fake codex shim");
    } else {
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"open {FAKE_URL} in your browser\"\nsleep 30\n"),
        )
        .expect("fake codex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    path
}

#[tokio::test]
async fn login_child() {
    let Ok(expected) = std::env::var("CODEX_LOGIN_EXPECTED") else {
        return;
    };
    let root = std::env::temp_dir().join(format!("zeron-codex-login-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let accounts = test_accounts(&root);
    let result = accounts.start_login(HarnessId::Codex).await;
    if expected == "true" {
        let start = result.expect("login starts");
        assert_eq!(start.mode, AgentLoginMode::Browser);
        assert_eq!(start.url, FAKE_URL);
        accounts.cancel_login(&start.login_id);
    } else {
        let error = result
            .expect_err("login must fail without a CLI")
            .to_string();
        assert!(
            error.contains("codex"),
            "error should name the missing CLI: {error}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn probe(path: &Path, override_path: Option<&Path>, expected: bool) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "login_child", "--nocapture"]);
    for key in [
        "HOME",
        "USERPROFILE",
        "FNM_DIR",
        "FNM_MULTISHELL_PATH",
        "LOCALAPPDATA",
        "NVM_SYMLINK",
        "VOLTA_HOME",
        "PNPM_HOME",
        "APPDATA",
        "CODEX_EXECUTABLE",
    ] {
        command.env_remove(key);
    }
    command
        .env("PATH", path)
        .env("CODEX_LOGIN_EXPECTED", expected.to_string())
        .env("ZERON_NO_LOGIN_SHELL", "1");
    if let Some(path) = override_path {
        command.env("CODEX_EXECUTABLE", path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An installation reachable only through `CODEX_EXECUTABLE` must log in —
/// the exact regression where Add account failed while the harness ran.
#[test]
fn codex_login_honors_override_only_installation() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_codex(dir.path());
    // No codex anywhere on PATH: only the override can start the login.
    probe(dir.path(), Some(&fake), true);
}

/// The npm layout — a `codex.cmd` shim on PATH with the payload buried in
/// `node_modules` — must log in through the same resolution the harness uses.
#[test]
fn codex_login_runs_the_npm_cmd_shim() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // The shim plays both roles itself.
    std::fs::copy(fake_codex(&bin), bin.join("codex.cmd")).unwrap();
    probe(&bin, None, true);
}

#[test]
fn codex_login_without_any_cli_fails_with_a_named_error() {
    if std::path::Path::new("/opt/homebrew/bin/codex").exists()
        || std::path::Path::new("/usr/local/bin/codex").exists()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    probe(dir.path(), None, false);
}
