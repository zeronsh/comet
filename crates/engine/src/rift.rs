//! Optional [Rift](https://github.com/anomalyco/rift) backend for isolated
//! session checkouts. Git worktrees stay the default; this path is an opt-in
//! copy-on-write clone of the source workspace, driven through the `rift` CLI
//! so Comet does not have to take Rift's rusqlite as a crate dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::EngineError;

/// Compiled-in support. Does not probe the current disk; a create still fails
/// when the filesystem cannot clone, or when `rift` is not on PATH.
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// Whether the `rift` binary is on PATH (or `ZERON_RIFT_BIN`).
pub fn available() -> bool {
    if !supported() {
        return false;
    }
    Command::new(rift_bin())
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn is_checkout(path: &Path) -> bool {
    path.join(".rift").is_file()
}

/// Marker Comet writes next to Rift's `.rift` id so claim/reuse can attribute
/// the clone back to the source repo without opening Rift's sqlite registry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutMarker {
    pub repo_path: String,
    #[serde(default)]
    pub isolation: zeron_proto::CheckoutIsolation,
}

pub const MARKER_FILE: &str = ".zeron-checkout.json";

pub fn read_marker(path: &Path) -> Option<CheckoutMarker> {
    let raw = std::fs::read_to_string(path.join(MARKER_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_marker(path: &Path, repo_path: &Path) -> Result<(), EngineError> {
    let marker = CheckoutMarker {
        repo_path: repo_path.to_string_lossy().into_owned(),
        isolation: zeron_proto::CheckoutIsolation::Rift,
    };
    let json = serde_json::to_string_pretty(&marker)
        .map_err(|e| EngineError::Other(format!("rift marker serialize: {e}")))?;
    std::fs::write(path.join(MARKER_FILE), json)?;
    hide_from_git(path, MARKER_FILE)?;
    Ok(())
}

fn hide_from_git(workspace: &Path, entry: &str) -> Result<(), EngineError> {
    let git = workspace.join(".git");
    if !git.is_dir() {
        return Ok(());
    }
    let info = git.join("info");
    std::fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let existing = match std::fs::read_to_string(&exclude) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let line = format!("/{entry}");
    if existing.lines().any(|row| row.trim() == line) {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(exclude, format!("{existing}{separator}{line}\n"))?;
    Ok(())
}

pub fn source_root(path: &Path) -> Option<String> {
    read_marker(path).map(|marker| marker.repo_path)
}

pub fn source_root_matches(path: &Path, repo_path: &Path) -> bool {
    let Some(root) = source_root(path) else {
        return false;
    };
    let marked = Path::new(&root);
    match (
        std::fs::canonicalize(marked),
        std::fs::canonicalize(repo_path),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => marked == repo_path,
    }
}

fn rift_bin() -> PathBuf {
    std::env::var_os("ZERON_RIFT_BIN")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rift"))
}

fn database_args() -> Vec<String> {
    match std::env::var("ZERON_RIFT_DATABASE") {
        Ok(path) if !path.is_empty() => vec!["--database".into(), path],
        _ => Vec::new(),
    }
}

fn run_rift(args: &[&str]) -> Result<String, EngineError> {
    let mut cmd = Command::new(rift_bin());
    cmd.args(database_args());
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    let output = cmd
        .output()
        .map_err(|e| EngineError::Other(format!("rift spawn failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = stderr.trim();
        let message = if message.is_empty() {
            stdout.trim()
        } else {
            message
        };
        return Err(EngineError::Other(if message.is_empty() {
            format!(
                "rift {} failed ({})",
                args.first().unwrap_or(&"?"),
                output.status
            )
        } else {
            format!("rift: {message}")
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn create(from: &Path, name: &str, into: &Path) -> Result<PathBuf, EngineError> {
    if !supported() {
        return Err(EngineError::Other(
            "Rift is not available on this platform".into(),
        ));
    }
    std::fs::create_dir_all(into)?;
    run_rift(&["init", "--here", &from.to_string_lossy()])?;
    let out = run_rift(&[
        "create",
        &from.to_string_lossy(),
        "--name",
        name,
        "--into",
        &into.to_string_lossy(),
        "--copy-all",
        "--no-hooks",
    ])?;
    let path = PathBuf::from(out.lines().last().unwrap_or(&out).trim());
    if path.as_os_str().is_empty() {
        return Err(EngineError::Other("rift create returned no path".into()));
    }
    Ok(path)
}

pub fn remove(path: &Path) -> Result<(), EngineError> {
    if !supported() {
        return Err(EngineError::Other(
            "Rift is not available on this platform".into(),
        ));
    }
    run_rift(&["remove", "--no-hooks", &path.to_string_lossy()])?;
    Ok(())
}
