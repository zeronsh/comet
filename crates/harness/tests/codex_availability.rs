//! Exercise the installed signal consumed by the engine registry and composer.
#![cfg(windows)]

use std::path::{Path, PathBuf};

use zeron_harness::{CodexHarness, Harness};

#[test]
fn availability_child() {
    let Ok(expected) = std::env::var("CODEX_AVAILABILITY_EXPECTED") else {
        return;
    };
    assert_eq!(CodexHarness::new().installed(), expected == "true");
}

fn probe(path: &Path, nvm: Option<&Path>, override_path: Option<&Path>, expected: bool) {
    probe_with_extra(path, nvm, override_path, &[], expected);
}

fn probe_with_extra(
    path: &Path,
    nvm: Option<&Path>,
    override_path: Option<&Path>,
    extra_env: &[(&str, PathBuf)],
    expected: bool,
) {
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
        "CODEX_EXECUTABLE",
    ] {
        command.env_remove(key);
    }
    command
        .env("PATH", path)
        .env("CODEX_AVAILABILITY_EXPECTED", expected.to_string());
    if let Some(nvm) = nvm {
        command.env("NVM_SYMLINK", nvm);
    }
    if let Some(path) = override_path {
        command.env("CODEX_EXECUTABLE", path);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn codex_availability_honors_native_override() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("custom.exe");
    std::fs::write(&exe, b"MZ").unwrap();
    // The npm shim is an equally valid override: it launches through cmd.exe.
    let shim = dir.path().join("codex.cmd");
    std::fs::write(&shim, b"@echo off\r\n").unwrap();
    probe(dir.path(), None, Some(&exe), true);
    probe(dir.path(), None, Some(&shim), true);
    // Overrides that do not exist do not count as installed.
    probe(
        dir.path(),
        None,
        Some(&dir.path().join("missing.exe")),
        false,
    );
}

/// npm installs expose `codex.cmd` on PATH (with the native payload buried in
/// `node_modules`); that shim is exactly what a bare `npm i -g` leaves behind.
#[test]
fn codex_availability_finds_npm_cmd_shim() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("npm-global");
    std::fs::create_dir_all(&prefix).unwrap();
    std::fs::write(prefix.join("codex.cmd"), b"npm shim").unwrap();
    // Found on PATH…
    probe(&prefix, None, None, true);
    // …and through the GUI-launch PATH backfill of npm's default global bin
    // (`%APPDATA%\npm`) when the process itself has a minimal PATH.
    std::fs::create_dir_all(prefix.join("npm")).unwrap();
    std::fs::write(prefix.join("npm").join("codex.cmd"), b"npm shim").unwrap();
    probe_with_extra(dir.path(), None, None, &[("APPDATA", prefix.clone())], true);
    std::fs::remove_file(prefix.join("codex.cmd")).unwrap();
    std::fs::remove_file(prefix.join("npm").join("codex.cmd")).unwrap();
    probe(&prefix, None, None, false);
}
