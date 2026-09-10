//! Exercise the installed signal consumed by the engine registry and composer.
#![cfg(windows)]

use zeron_harness::{CodexHarness, Harness};

#[test]
fn availability_child() {
    let Ok(expected) = std::env::var("CODEX_AVAILABILITY_EXPECTED") else {
        return;
    };
    assert_eq!(CodexHarness::new().installed(), expected == "true");
}

fn probe(
    path: &std::path::Path,
    nvm: Option<&std::path::Path>,
    override_path: Option<&std::path::Path>,
    expected: bool,
) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "availability_child", "--nocapture"]);
    for key in [
        "HOME",
        "USERPROFILE",
        "FNM_DIR",
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
    probe(dir.path(), None, Some(&exe), true);
}

#[test]
fn codex_availability_finds_npm_payload() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("Node Current");
    let triple = if cfg!(target_arch = "aarch64") {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    for nested in [true, false] {
        let package = prefix.join("node_modules/@openai/codex");
        let platform = if nested {
            package.join(format!("node_modules/@openai/codex-win32-{arch}"))
        } else {
            prefix.join(format!("node_modules/@openai/codex-win32-{arch}"))
        };
        let exe = platform.join("vendor").join(triple).join("bin/codex.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"MZ").unwrap();
        std::fs::write(prefix.join("codex.cmd"), b"npm shim").unwrap();
        probe(&prefix, None, None, true);
        probe(dir.path(), Some(&prefix), None, true);
        std::fs::remove_file(exe).unwrap();
    }
    probe(&prefix, None, None, false);
}

#[test]
fn codex_availability_rejects_batch_override_even_with_path_exe() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("codex.exe"), b"MZ").unwrap();
    probe(dir.path(), None, Some(&dir.path().join("codex.cmd")), false);
}
