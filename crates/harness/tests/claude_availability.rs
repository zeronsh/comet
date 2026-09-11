//! The `installed` signal honors the executable override, matching the launch
//! resolver (the registry and composer must not disable an agent that launches).
//!
//! Child-process probes: resolution reads process env (PATH, the override), so
//! each case re-runs this test binary with a controlled environment.

use zeron_harness::{ClaudeHarness, Harness};

#[test]
fn availability_child() {
    let Ok(expected) = std::env::var("CLAUDE_AVAILABILITY_EXPECTED") else {
        return;
    };
    assert_eq!(
        ClaudeHarness::new().installed(),
        expected == "true",
        "installed() must agree with the launch resolver"
    );
}

fn probe(path: &std::path::Path, override_path: Option<&std::path::Path>, expected: bool) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "availability_child", "--nocapture"]);
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
        "CLAUDE_CODE_EXECUTABLE",
    ] {
        command.env_remove(key);
    }
    // The login-shell PATH snapshot would otherwise carry this machine's shell.
    command.env("ZERON_NO_LOGIN_SHELL", "1");
    command
        .env("PATH", path)
        .env("CLAUDE_AVAILABILITY_EXPECTED", expected.to_string());
    if let Some(path) = override_path {
        command.env("CLAUDE_CODE_EXECUTABLE", path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `installed()` is true when Claude is reachable only through a valid
/// override — the exact regression where the registry reported `false` while
/// launches worked.
#[test]
fn claude_availability_honors_override_without_path_cli() {
    let dir = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) {
        "claude-custom.exe"
    } else {
        "claude-custom"
    };
    let exe = dir.path().join(name);
    std::fs::write(&exe, if cfg!(windows) { b"MZ" } else { b"sh" }).unwrap();
    // PATH without any claude: only the override makes Claude installed.
    probe(dir.path(), Some(&exe), true);
    probe(dir.path(), None, false);
}

#[test]
fn claude_availability_reports_not_installed_without_any_cli() {
    // Absolute Unix fallback locations would defeat the empty-PATH probe on
    // machines that really have a claude there.
    if std::path::Path::new("/opt/homebrew/bin/claude").exists()
        || std::path::Path::new("/usr/local/bin/claude").exists()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    probe(dir.path(), None, false);
}
