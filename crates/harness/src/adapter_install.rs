//! Managed installs for npm-distributed ACP adapters.
//!
//! The old fallback spawned `npx -y <pkg>` at chat time, which put every
//! user's npm state in the hot path: a cold cache meant a multi-minute
//! download while the chat showed "Working", and a broken one meant npm dying
//! before the adapter ever ran — silently, with an errno-encoded exit code
//! (254 = ENOENT, the zeronsh/comet#95 crash) that surfaced as an opaque
//! "harness protocol error". Instead, pinned adapter packages are installed
//! ONCE into a zeron-owned prefix (`~/.zeron/adapters/<pkg>/<version>`, own
//! npm cache beside it, so a root-owned or read-only `~/.npm` can't break
//! us), and every subsequent launch spawns `node <entry>` directly — no npm
//! anywhere near a chat turn.
//!
//! Install is atomic: npm runs in a `.tmp-*` sibling which is renamed into
//! place only after the bin entry resolves and a marker file is written, so a
//! killed install can never masquerade as a working adapter, and concurrent
//! installers (two daemons, prewarm racing a run) converge on whichever
//! rename won.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::HarnessError;

/// A pinned npm package: `"@scope/name@1.2.3"` → name + version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NpmPin {
    pub name: &'static str,
    pub version: &'static str,
}

impl NpmPin {
    /// Split a `name@version` pin at the LAST `@` (scoped names carry a
    /// leading one).
    pub(crate) fn parse(pin: &'static str) -> Self {
        match pin.rfind('@') {
            Some(at) if at > 0 => Self {
                name: &pin[..at],
                version: &pin[at + 1..],
            },
            _ => Self {
                name: pin,
                version: "latest",
            },
        }
    }

    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Filesystem-safe directory name (`@scope/name` → `scope__name`).
    fn dir_name(&self) -> String {
        self.name.trim_start_matches('@').replace('/', "__")
    }
}

const OK_MARKER: &str = ".zeron-install-ok";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// `$ZERON_ADAPTERS_DIR`, else `~/.zeron/adapters`.
fn adapters_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ZERON_ADAPTERS_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".zeron").join("adapters"))
}

fn install_dir(pin: &NpmPin) -> Option<PathBuf> {
    adapters_root().map(|root| root.join(pin.dir_name()).join(pin.version))
}

/// The package's bin entry inside an install dir, from its own package.json
/// (`bin` as a string, or a map preferring `bin_name`).
fn bin_entry(dir: &Path, pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let pkg_dir = dir.join("node_modules").join(pin.name);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pkg_dir.join("package.json")).ok()?).ok()?;
    let rel = match manifest.get("bin")? {
        serde_json::Value::String(entry) => entry.clone(),
        serde_json::Value::Object(map) => map
            .get(bin_name)
            .or_else(|| map.values().next())
            .and_then(|v| v.as_str())
            .map(str::to_owned)?,
        _ => return None,
    };
    let entry = pkg_dir.join(rel);
    entry.exists().then_some(entry)
}

/// The bin entry of a COMPLETED managed install, `None` when absent.
pub(crate) fn installed_entry(pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let dir = install_dir(pin)?;
    if !dir.join(OK_MARKER).exists() {
        return None;
    }
    bin_entry(&dir, pin, bin_name)
}

pub(crate) fn find_npm() -> Option<PathBuf> {
    crate::acp::find_on_paths("npm", Vec::new())
}

/// How to spawn an installed entry: JS entries (the overwhelming npm norm,
/// shebang or not) run via `node`; a native binary published as a bin entry
/// runs directly.
pub(crate) fn launch_for_entry(entry: &Path) -> Result<(PathBuf, Vec<String>), HarnessError> {
    let head = std::fs::read(entry)
        .ok()
        .map(|b| b.into_iter().take(4).collect::<Vec<u8>>())
        .unwrap_or_default();
    let native = head.starts_with(b"\x7fELF")
        || head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || head.starts_with(&[0xca, 0xfe, 0xba, 0xbe]);
    if native {
        return Ok((entry.to_path_buf(), Vec::new()));
    }
    // Prefer node beside npm (version managers keep them together); PATH and
    // the login-shell snapshot cover the rest.
    let extra = find_npm()
        .and_then(|npm| npm.parent().map(|d| d.join("node")))
        .into_iter()
        .collect();
    let node = crate::acp::find_on_paths("node", extra).ok_or_else(|| {
        HarnessError::NotInstalled(
            "node (required to run the agent's npm-distributed ACP adapter; \
             searched PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun \
             install dirs)"
                .into(),
        )
    })?;
    Ok((node, vec![entry.display().to_string()]))
}

/// npm encodes fatal fs errors as `256 - errno` (npm/cli#4838 — often with no
/// stderr at all); name the ones users actually hit.
fn describe_npm_exit(status: Option<std::process::ExitStatus>) -> String {
    let base = crate::describe_exit(status);
    let hint = match status.and_then(|s| s.code()) {
        Some(254) => Some("ENOENT — a file or directory npm needed is missing"),
        Some(243) => Some("EACCES — permission denied, often a root-owned or unwritable npm dir"),
        Some(226) => Some("EROFS — read-only filesystem"),
        _ => None,
    };
    match hint {
        Some(hint) => format!("{base}, {hint}"),
        None => base,
    }
}

/// One installer at a time per process; installs are rare and npm handles
/// its own intra-install parallelism.
fn install_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Ensure the pinned package is installed; returns its bin entry. Failures
/// carry npm's own output — the whole point is that a dying npm stops being
/// an undiagnosable one-liner.
pub(crate) async fn ensure_installed(
    pin: NpmPin,
    bin_name: &str,
    display_name: &str,
) -> Result<PathBuf, HarnessError> {
    if let Some(entry) = installed_entry(&pin, bin_name) {
        return Ok(entry);
    }
    let _guard = install_lock().lock().await;
    if let Some(entry) = installed_entry(&pin, bin_name) {
        return Ok(entry);
    }

    let Some(npm) = find_npm() else {
        return Err(HarnessError::NotInstalled(format!(
            "npm (required to install the {display_name} ACP adapter {}; searched \
             PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun install dirs)",
            pin.spec()
        )));
    };
    let root = adapters_root().ok_or_else(|| {
        HarnessError::Install("cannot locate an adapters directory (HOME is unset)".into())
    })?;
    let final_dir = install_dir(&pin).expect("root resolved");
    let tmp_dir = root.join(format!(
        ".tmp-{}-{}-{}",
        pin.dir_name(),
        pin.version,
        std::process::id()
    ));
    let cache_dir = root.join(".npm-cache");
    let install = install_into(&npm, &pin, &tmp_dir, &cache_dir, display_name).await;
    if let Err(e) = install {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }
    if bin_entry(&tmp_dir, &pin, bin_name).is_none() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(HarnessError::Install(format!(
            "npm install {} completed but the package has no runnable bin entry",
            pin.spec()
        )));
    }
    std::fs::write(tmp_dir.join(OK_MARKER), pin.version)?;
    if let Some(parent) = final_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&tmp_dir, &final_dir).is_err() {
        // Lost a cross-process race (or a stale dir): keep whatever is in
        // place if it's complete, else replace it.
        if installed_entry(&pin, bin_name).is_none() {
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp_dir, &final_dir)?;
        } else {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }
    installed_entry(&pin, bin_name).ok_or_else(|| {
        HarnessError::Install(format!(
            "install of {} finished but its bin entry did not resolve",
            pin.spec()
        ))
    })
}

/// A zeron-owned shim script materialized INSIDE a managed install dir, for
/// SDK packages with no bin entry (`@cursor/sdk`): the shim resolves the SDK
/// from the sibling `node_modules`. Returns the shim path when the install is
/// complete AND the shim contents match this build (a comet upgrade that
/// changes the shim rewrites it in place).
pub(crate) fn installed_shim(pin: &NpmPin, shim_name: &str, contents: &str) -> Option<PathBuf> {
    let dir = install_dir(pin)?;
    if !dir.join(OK_MARKER).exists() {
        return None;
    }
    let shim = dir.join(shim_name);
    match std::fs::read_to_string(&shim) {
        Ok(existing) if existing == contents => Some(shim),
        _ => {
            std::fs::write(&shim, contents).ok()?;
            Some(shim)
        }
    }
}

/// Like [`ensure_installed`], for a package consumed as a LIBRARY by a
/// zeron-owned shim rather than through a bin entry. Installs the pin once,
/// writes `contents` as `<install-dir>/<shim_name>`, and returns the shim
/// path (spawn it via [`launch_for_entry`]).
pub(crate) async fn ensure_installed_shim(
    pin: NpmPin,
    display_name: &str,
    shim_name: &str,
    contents: &str,
) -> Result<PathBuf, HarnessError> {
    if let Some(shim) = installed_shim(&pin, shim_name, contents) {
        return Ok(shim);
    }
    let _guard = install_lock().lock().await;
    if let Some(shim) = installed_shim(&pin, shim_name, contents) {
        return Ok(shim);
    }

    let Some(npm) = find_npm() else {
        return Err(HarnessError::NotInstalled(format!(
            "npm (required to install the {display_name} SDK {}; searched \
             PATH, the login shell's PATH, and fnm/nvm/volta/pnpm/bun install dirs)",
            pin.spec()
        )));
    };
    let root = adapters_root().ok_or_else(|| {
        HarnessError::Install("cannot locate an adapters directory (HOME is unset)".into())
    })?;
    let final_dir = install_dir(&pin).expect("root resolved");
    let tmp_dir = root.join(format!(
        ".tmp-{}-{}-{}",
        pin.dir_name(),
        pin.version,
        std::process::id()
    ));
    let cache_dir = root.join(".npm-cache");
    let install = install_into(&npm, &pin, &tmp_dir, &cache_dir, display_name).await;
    if let Err(e) = install {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }
    std::fs::write(tmp_dir.join(shim_name), contents)?;
    std::fs::write(tmp_dir.join(OK_MARKER), pin.version)?;
    if let Some(parent) = final_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&tmp_dir, &final_dir).is_err() {
        // Lost a cross-process race (or a stale dir): keep whatever is in
        // place if it's complete, else replace it.
        if !final_dir.join(OK_MARKER).exists() {
            let _ = std::fs::remove_dir_all(&final_dir);
            std::fs::rename(&tmp_dir, &final_dir)?;
        } else {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }
    installed_shim(&pin, shim_name, contents).ok_or_else(|| {
        HarnessError::Install(format!(
            "install of {} finished but its shim did not resolve",
            pin.spec()
        ))
    })
}

async fn install_into(
    npm: &Path,
    pin: &NpmPin,
    tmp_dir: &Path,
    cache_dir: &Path,
    display_name: &str,
) -> Result<(), HarnessError> {
    let _ = std::fs::remove_dir_all(tmp_dir);
    std::fs::create_dir_all(tmp_dir)?;
    std::fs::create_dir_all(cache_dir)?;
    // A bare manifest keeps npm from walking up into a user project.
    std::fs::write(tmp_dir.join("package.json"), "{\"private\":true}\n")?;
    tracing::info!(
        target: "zeron_harness::adapter_install",
        package = %pin.spec(),
        dir = %tmp_dir.display(),
        "installing ACP adapter"
    );
    let mut cmd = tokio::process::Command::new(npm);
    cmd.args([
        "install",
        "--no-audit",
        "--no-fund",
        "--no-progress",
        "--loglevel=error",
        // Defeat a user-level `omit=optional`: @openai/codex ships its
        // platform binary as an optional dependency.
        "--include=optional",
        "--cache",
    ])
    .arg(cache_dir)
    .arg(pin.spec())
    .current_dir(tmp_dir)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    crate::compose_child_path(&mut cmd, npm);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = async {
        use tokio::io::AsyncReadExt;
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out).await;
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut err).await;
        }
        (out, err)
    };
    let ((out, err), status) =
        match tokio::time::timeout(INSTALL_TIMEOUT, async { tokio::join!(drain, child.wait()) })
            .await
        {
            Ok((streams, status)) => (streams, status),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(HarnessError::Install(format!(
                    "npm install of the {display_name} adapter ({}) timed out after {} minutes — \
                 check your network and npm registry configuration",
                    pin.spec(),
                    INSTALL_TIMEOUT.as_secs() / 60
                )));
            }
        };
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let mut output = err.trim().to_owned();
    if output.is_empty() {
        output = out.trim().to_owned();
    }
    let tail: String = if output.len() > 1200 {
        // npm front-loads "npm ERR!" lines; keep the tail where the cause lands.
        format!("…{}", &output[output.len() - 1200..])
    } else {
        output
    };
    let exit = describe_npm_exit(Some(status));
    Err(HarnessError::Install(if tail.is_empty() {
        format!(
            "npm install of the {display_name} adapter ({}) failed silently ({exit}); \
             npm's own cache or config is likely broken — try `npm cache verify` \
             or reinstalling node/npm",
            pin.spec()
        )
    } else {
        format!(
            "npm install of the {display_name} adapter ({}) failed ({exit}): {tail}",
            pin.spec()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_parses_scoped_and_bare_names() {
        let pin = NpmPin::parse("@agentclientprotocol/codex-acp@1.1.14");
        assert_eq!(pin.name, "@agentclientprotocol/codex-acp");
        assert_eq!(pin.version, "1.1.14");
        assert_eq!(pin.dir_name(), "agentclientprotocol__codex-acp");

        let pin = NpmPin::parse("pi-acp@0.0.33");
        assert_eq!(pin.name, "pi-acp");
        assert_eq!(pin.version, "0.0.33");
        assert_eq!(pin.dir_name(), "pi-acp");
    }

    #[test]
    fn npm_errno_exits_are_decoded() {
        use std::os::unix::process::ExitStatusExt;
        let status = |code: i32| Some(std::process::ExitStatus::from_raw(code << 8));
        assert!(describe_npm_exit(status(254)).contains("ENOENT"));
        assert!(describe_npm_exit(status(243)).contains("EACCES"));
        assert!(describe_npm_exit(status(226)).contains("EROFS"));
        assert_eq!(describe_npm_exit(status(1)), "exit code 1");
    }

    #[test]
    fn bin_entry_reads_string_and_map_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let pin = NpmPin::parse("@scope/tool@1.0.0");
        let pkg = dir.path().join("node_modules").join("@scope").join("tool");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("dist.js"), "x").unwrap();

        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":"dist.js"}"#,
        )
        .unwrap();
        assert_eq!(
            bin_entry(dir.path(), &pin, "tool"),
            Some(pkg.join("dist.js"))
        );

        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":{"tool":"dist.js","other":"missing.js"}}"#,
        )
        .unwrap();
        assert_eq!(
            bin_entry(dir.path(), &pin, "tool"),
            Some(pkg.join("dist.js"))
        );

        // Marker gating: entry present but no marker → not installed.
        assert_eq!(
            install_dir(&pin).is_some(),
            std::env::var_os("HOME").is_some()
        );
    }
}
