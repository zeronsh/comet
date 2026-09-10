//! Managed installs for npm-distributed ACP adapters.
//!
//! The old fallback spawned `npx -y <pkg>` at chat time, which put every
//! user's npm state in the hot path: a cold cache meant a multi-minute
//! download while the chat showed "Working", and a broken one meant npm dying
//! before the adapter ever ran — silently, with an errno-encoded exit code
//! (254 = ENOENT, the zeronsh/comet#95 crash) that surfaced as an opaque
//! "harness protocol error". Instead, pinned adapter packages are installed
//! ONCE into a zeron-owned prefix (`~/.zeron/adapters/<pkg>/<version>` on
//! Unix, the local app-data directory on Windows), with its own npm cache
//! beside it, so a root-owned or read-only user cache cannot break us. Every
//! subsequent launch spawns `node <entry>` directly — no npm anywhere near a
//! chat turn.
//!
//! Install is atomic: npm runs in a `.tmp-*` sibling which is renamed into
//! place only after the bin entry resolves and a marker file is written, so a
//! killed install can never masquerade as a working adapter, and concurrent
//! installers (two daemons, prewarm racing a run) converge on whichever
//! rename won.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::HarnessError;
use crate::process::Stdio;

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

/// Managed adapter storage. `$ZERON_ADAPTERS_DIR` wins, followed by
/// `$ZERON_DATA_DIR/adapters`. Windows defaults to
/// `%LOCALAPPDATA%/Zeron/adapters` (or `%USERPROFILE%/AppData/Local/...`);
/// Unix keeps `~/.zeron/adapters`.
fn adapters_root() -> Option<PathBuf> {
    adapters_root_with(
        &|key| std::env::var_os(key),
        crate::executable::Platform::current(),
    )
}

fn adapters_root_with(
    env: &impl Fn(&str) -> Option<OsString>,
    platform: crate::executable::Platform,
) -> Option<PathBuf> {
    let value = |key| {
        env(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(dir) = value("ZERON_ADAPTERS_DIR") {
        return Some(dir);
    }
    if let Some(dir) = value("ZERON_DATA_DIR") {
        return Some(dir.join("adapters"));
    }
    if platform == crate::executable::Platform::Windows {
        value("LOCALAPPDATA")
            .map(|dir| dir.join("Zeron").join("adapters"))
            .or_else(|| {
                value("USERPROFILE").map(|home| {
                    home.join("AppData")
                        .join("Local")
                        .join("Zeron")
                        .join("adapters")
                })
            })
    } else {
        value("HOME").map(|home| home.join(".zeron").join("adapters"))
    }
}

fn install_dir_in(root: &Path, pin: &NpmPin) -> PathBuf {
    root.join(pin.dir_name()).join(pin.version)
}

fn install_dir(pin: &NpmPin) -> Option<PathBuf> {
    adapters_root().map(|root| install_dir_in(&root, pin))
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
    entry.is_file().then_some(entry)
}

/// The bin entry of a COMPLETED managed install, `None` when absent.
pub(crate) fn installed_entry(pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let root = adapters_root()?;
    installed_entry_in(&root, pin, bin_name)
}

fn installed_entry_in(root: &Path, pin: &NpmPin, bin_name: &str) -> Option<PathBuf> {
    let dir = install_dir_in(root, pin);
    if !dir.join(OK_MARKER).is_file() {
        return None;
    }
    bin_entry(&dir, pin, bin_name)
}

fn npm_cli_for_node(node: &Path) -> Option<PathBuf> {
    let cli = node
        .parent()?
        .join("node_modules")
        .join("npm")
        .join("bin")
        .join("npm-cli.js");
    cli.is_file().then_some(cli)
}

fn find_npm_with(
    env: &impl Fn(&str) -> Option<OsString>,
    login_shell_path: Option<OsString>,
    platform: crate::executable::Platform,
) -> Option<PathBuf> {
    if platform == crate::executable::Platform::Windows {
        let node = crate::executable::find_on_paths_matching_with(
            "node",
            Vec::new(),
            env,
            login_shell_path,
            platform,
            |node| npm_cli_for_node(node).is_some(),
        )?;
        npm_cli_for_node(&node)
    } else {
        crate::executable::find_on_paths_with("npm", Vec::new(), env, login_shell_path, platform)
    }
}

pub(crate) fn find_npm() -> Option<PathBuf> {
    find_npm_with(
        &|key| std::env::var_os(key),
        crate::shell_env::login_shell_path().map(OsString::from),
        crate::executable::Platform::current(),
    )
}

fn node_sibling_for_npm(npm: &Path, platform: crate::executable::Platform) -> Option<PathBuf> {
    if platform == crate::executable::Platform::Windows {
        // <node-root>/node_modules/npm/bin/npm-cli.js -> <node-root>/node.exe
        npm.parent()?
            .parent()?
            .parent()?
            .parent()
            .map(|root| root.join("node.exe"))
            .filter(|node| node.is_file())
    } else {
        npm.parent()
            .map(|dir| dir.join("node"))
            .filter(|node| node.is_file())
    }
}

fn is_native_executable(entry: &Path) -> Result<bool, HarnessError> {
    const MAX_PE_HEADER_OFFSET: u64 = 1024 * 1024;
    let mut file = std::fs::File::open(entry)?;
    let mut head = [0_u8; 4];
    let count = file.read(&mut head)?;
    let head = &head[..count];
    if head.starts_with(b"\x7fELF")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
        ]
        .iter()
        .any(|magic| head.starts_with(magic))
    {
        return Ok(true);
    }
    if !head.starts_with(b"MZ") {
        return Ok(false);
    }

    file.seek(SeekFrom::Start(0x3c))?;
    let mut offset = [0_u8; 4];
    if file.read(&mut offset)? != offset.len() {
        return Ok(false);
    }
    let offset = u32::from_le_bytes(offset) as u64;
    let length = file.metadata()?.len();
    if offset > MAX_PE_HEADER_OFFSET || offset.saturating_add(4) > length {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut signature = [0_u8; 4];
    Ok(file.read(&mut signature)? == signature.len() && signature == *b"PE\0\0")
}

/// How to spawn an installed entry: JavaScript runs via `node`; a native ELF,
/// Mach-O, or PE bin entry runs directly. Managed batch wrappers are rejected:
/// they require cmd.exe and would destroy exact argument boundaries.
pub(crate) fn launch_for_entry(entry: &Path) -> Result<(PathBuf, Vec<String>), HarnessError> {
    launch_for_entry_with_node(entry, None)
}

fn launch_for_entry_with_node(
    entry: &Path,
    supplied_node: Option<PathBuf>,
) -> Result<(PathBuf, Vec<String>), HarnessError> {
    if entry
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    {
        return Err(HarnessError::Install(format!(
            "managed adapter entry {} is a Windows batch wrapper; install a package exposing a JavaScript entry or native .exe instead",
            entry.display()
        )));
    }
    if is_native_executable(entry)? {
        return Ok((entry.to_path_buf(), Vec::new()));
    }

    let platform = crate::executable::Platform::current();
    let sibling = find_npm().and_then(|npm| node_sibling_for_npm(&npm, platform));
    let node = supplied_node
        .or(sibling)
        .or_else(|| crate::executable::find_on_paths("node", Vec::new()))
        .ok_or_else(|| {
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
        HarnessError::Install("cannot locate a platform data directory for managed adapters".into())
    })?;
    let final_dir = install_dir_in(&root, &pin);
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
        HarnessError::Install("cannot locate a platform data directory for managed adapters".into())
    })?;
    let final_dir = install_dir_in(&root, &pin);
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

fn npm_install_plan(
    npm: &Path,
    pin: &NpmPin,
    cache_dir: &Path,
    platform: crate::executable::Platform,
) -> Result<(PathBuf, Vec<OsString>), HarnessError> {
    let (program, mut args) = if platform == crate::executable::Platform::Windows {
        let node = node_sibling_for_npm(npm, platform).ok_or_else(|| {
            HarnessError::NotInstalled(format!(
                "node.exe beside the discovered npm CLI {}",
                npm.display()
            ))
        })?;
        (node, vec![npm.as_os_str().to_owned()])
    } else {
        (npm.to_path_buf(), Vec::new())
    };
    args.extend(
        [
            "install",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--loglevel=error",
            // Defeat a user-level `omit=optional`: packages can ship their
            // platform binary as an optional dependency.
            "--include=optional",
            "--cache",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(cache_dir.as_os_str().to_owned());
    args.push(OsString::from(pin.spec()));
    Ok((program, args))
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
    let (program, args) =
        npm_install_plan(npm, pin, cache_dir, crate::executable::Platform::current())?;
    let mut cmd = crate::process::Command::new(&program);
    cmd.args(args)
        .current_dir(tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::compose_child_path(&mut cmd, &program);
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

    use std::collections::HashMap;

    fn env(values: &[(&str, OsString)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let values: HashMap<String, OsString> = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        move |key| values.get(key).cloned()
    }

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
        #[cfg(unix)]
        let status = |code: i32| {
            use std::os::unix::process::ExitStatusExt;
            Some(std::process::ExitStatus::from_raw(code << 8))
        };
        #[cfg(windows)]
        let status = |code: i32| {
            use std::os::windows::process::ExitStatusExt;
            Some(std::process::ExitStatus::from_raw(code as u32))
        };
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

        // Marker gating uses an isolated managed root, never the process HOME.
        let root = dir.path().join("isolated-adapters");
        let installed = install_dir_in(&root, &pin);
        let installed_pkg = installed.join("node_modules").join("@scope").join("tool");
        std::fs::create_dir_all(&installed_pkg).unwrap();
        std::fs::write(installed_pkg.join("dist.js"), "x").unwrap();
        std::fs::write(
            installed_pkg.join("package.json"),
            r#"{"name":"@scope/tool","bin":"dist.js"}"#,
        )
        .unwrap();
        assert_eq!(installed_entry_in(&root, &pin, "tool"), None);
        std::fs::write(installed.join(OK_MARKER), pin.version).unwrap();
        assert_eq!(
            installed_entry_in(&root, &pin, "tool"),
            Some(installed_pkg.join("dist.js"))
        );
    }

    #[test]
    fn adapters_root_has_injected_windows_precedence_and_fallbacks() {
        let explicit = PathBuf::from(r"D:\Zeron adapters");
        let data = PathBuf::from(r"E:\Zeron data");
        let local = PathBuf::from(r"C:\Users\Ada\AppData\Local");
        let profile = PathBuf::from(r"C:\Users\Ada");
        assert_eq!(
            adapters_root_with(
                &env(&[
                    ("ZERON_ADAPTERS_DIR", explicit.clone().into_os_string()),
                    ("ZERON_DATA_DIR", data.clone().into_os_string()),
                    ("LOCALAPPDATA", local.clone().into_os_string()),
                ]),
                crate::executable::Platform::Windows,
            ),
            Some(explicit)
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("ZERON_DATA_DIR", data.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(data.join("adapters"))
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("LOCALAPPDATA", local.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(local.join("Zeron").join("adapters"))
        );
        assert_eq!(
            adapters_root_with(
                &env(&[("USERPROFILE", profile.clone().into_os_string())]),
                crate::executable::Platform::Windows,
            ),
            Some(
                profile
                    .join("AppData")
                    .join("Local")
                    .join("Zeron")
                    .join("adapters")
            )
        );
    }

    #[test]
    fn windows_npm_discovery_falls_back_to_fnm_default_with_complete_node_layout() {
        let temp = tempfile::tempdir().unwrap();
        let roaming = temp.path().join("Roaming with spaces");
        let active = temp.path().join("active");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(active.join("node.exe"), b"MZ").unwrap();
        let root = roaming.join("fnm/aliases/default");
        let npm = root.join("node_modules/npm/bin/npm-cli.js");
        std::fs::create_dir_all(npm.parent().unwrap()).unwrap();
        std::fs::write(root.join("node.exe"), b"MZ").unwrap();
        std::fs::write(&npm, b"// fixture").unwrap();
        let lookup = env(&[
            ("APPDATA", roaming.into_os_string()),
            ("FNM_MULTISHELL_PATH", active.into_os_string()),
        ]);
        let platform = crate::executable::Platform::Windows;
        assert_eq!(find_npm_with(&lookup, None, platform), Some(npm.clone()));
        assert_eq!(
            node_sibling_for_npm(&npm, platform),
            Some(root.join("node.exe"))
        );
        std::fs::remove_file(root.join("node.exe")).unwrap();
        assert_eq!(find_npm_with(&lookup, None, platform), None);
    }

    #[test]
    fn windows_npm_discovery_and_install_plan_use_node_without_a_shell() {
        let temp = tempfile::tempdir().unwrap();
        let node_root = temp.path().join("Node's Ω & ^ % !");
        let node = node_root.join("node.exe");
        let npm = node_root
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(npm.parent().unwrap()).unwrap();
        std::fs::write(&node, b"MZ").unwrap();
        std::fs::write(&npm, b"console.log('npm')").unwrap();
        // A batch shim exists but is deliberately never selected.
        std::fs::write(node_root.join("npm.cmd"), b"@echo off").unwrap();
        let path = std::env::join_paths([&node_root]).unwrap();
        let discovered = find_npm_with(
            &env(&[("PATH", path)]),
            None,
            crate::executable::Platform::Windows,
        );
        assert_eq!(discovered, Some(npm.clone()));

        let cache = temp.path().join("cache's Ω & $(echo nope); ^ % !");
        let pin = NpmPin::parse("@scope/tool@1.2.3");
        let (program, args) =
            npm_install_plan(&npm, &pin, &cache, crate::executable::Platform::Windows).unwrap();
        assert_eq!(program, node);
        assert_eq!(args.first(), Some(&npm.into_os_string()));
        assert_eq!(args[1], OsString::from("install"));
        assert_eq!(args[8], cache.into_os_string());
        assert_eq!(args[9], OsString::from("@scope/tool@1.2.3"));
        assert_eq!(args.len(), 10);
    }

    #[test]
    fn managed_entry_launch_recognizes_pe_and_preserves_javascript_argument_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let pe = temp.path().join("adapter.exe");
        let mut bytes = vec![0_u8; 0x84];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        std::fs::write(&pe, bytes).unwrap();
        assert_eq!(
            launch_for_entry_with_node(&pe, None).unwrap(),
            (pe, Vec::new())
        );

        let js = temp.path().join("adapter's Ω & $(nope); ^ % !.js");
        let node = temp.path().join("node.exe");
        std::fs::write(&js, b"#!/usr/bin/env node\n").unwrap();
        let launch = launch_for_entry_with_node(&js, Some(node.clone())).unwrap();
        assert_eq!(launch.0, node);
        assert_eq!(launch.1, vec![js.display().to_string()]);
    }

    #[test]
    fn managed_batch_entries_are_rejected_actionably() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["adapter.cmd", "adapter.BAT"] {
            let entry = temp.path().join(name);
            std::fs::write(&entry, b"@echo off").unwrap();
            let error = launch_for_entry_with_node(&entry, None).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("Windows batch wrapper"));
            assert!(message.contains("native .exe"));
        }
    }
}
