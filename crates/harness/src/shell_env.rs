//! Login-shell PATH snapshot.
//!
//! GUI/service launches (Dock, Finder, launchd, systemd) never run the user's
//! shell init, so the daemon's own PATH misses everything the shell shapes:
//! nvm's shell function, fnm multishells, asdf/mise shims, custom npm
//! prefixes, nix profiles, `~/.zshrc` exports. The hardcoded known-location
//! lists in the resolvers cover the common managers, but the only fix that
//! works for *any* setup is asking the user's actual shell: spawn it once as
//! an interactive login shell, have it print its environment between markers,
//! and keep the PATH it reports. If `codex`/`claude` runs in their terminal,
//! it resolves here too.
//!
//! The snapshot is captured once per process (cached, including a negative
//! result) and is defensive about hostile shell init:
//! - `-lic` first (interactive login — nvm and friends load in rc files),
//!   falling back to `-lc` if that produces nothing (some rc files hang or
//!   `exec` a multiplexer when interactive).
//! - Output is read on a side thread into a shared buffer; the poll loop
//!   returns as soon as the end marker appears, so init that blocks *after*
//!   printing (or grandchildren inheriting the pipe) can't wedge us.
//! - A hard per-attempt timeout kills the shell.
//!
//! Set `COMET_NO_LOGIN_SHELL=1` to disable the snapshot entirely.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

static CACHE: OnceLock<Option<OsString>> = OnceLock::new();

/// The PATH the user's login shell reports, captured once and cached for the
/// life of the process. `None` when disabled, non-unix, no usable shell, or
/// the shell never produced a parseable snapshot.
pub fn login_shell_path() -> Option<&'static OsStr> {
    #[cfg(unix)]
    {
        CACHE.get_or_init(unix::capture).as_deref()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Kick off the snapshot on a background thread so the first harness resolve
/// doesn't pay the shell-startup latency inline. Call at daemon startup.
pub fn prewarm() {
    #[cfg(unix)]
    {
        let _ = std::thread::Builder::new()
            .name("comet-shell-env".into())
            .spawn(|| {
                let _ = login_shell_path();
            });
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::io::Read;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const BEGIN_MARKER: &str = "__COMET_SHELL_ENV_BEGIN__";
    const END_MARKER: &str = "__COMET_SHELL_ENV_END__";
    /// Enough for any sane environment; a runaway rc file can't OOM us.
    const MAX_OUTPUT: usize = 2 * 1024 * 1024;
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
    /// After the shell exits, wait this long for the pipe to flush.
    const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(250);

    pub(super) fn capture() -> Option<OsString> {
        if std::env::var_os("COMET_NO_LOGIN_SHELL").is_some_and(|v| !v.is_empty()) {
            return None;
        }
        let shell = user_shell()?;
        snapshot_path(&shell, ATTEMPT_TIMEOUT)
    }

    /// The user's shell: `$SHELL`, then the passwd entry, then well-known
    /// defaults. Non-executables and nologin shells are skipped.
    fn user_shell() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            candidates.push(PathBuf::from(s));
        }
        // systemd/launchd services often start without SHELL — passwd has it.
        if let Some(p) = passwd_shell() {
            candidates.push(p);
        }
        candidates.push(PathBuf::from("/bin/zsh"));
        candidates.push(PathBuf::from("/bin/bash"));
        candidates.push(PathBuf::from("/bin/sh"));
        candidates.into_iter().find(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            let blocked = matches!(name.as_deref(), Some("nologin" | "false") | None);
            !blocked && is_executable(p)
        })
    }

    fn passwd_shell() -> Option<PathBuf> {
        // SAFETY: getpwuid's static buffer is only read here, and callers are
        // serialized through the OnceLock init above.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() || (*pw).pw_shell.is_null() {
                return None;
            }
            let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
            (!shell.to_bytes().is_empty())
                .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(shell.to_bytes())))
        }
    }

    fn is_executable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    /// Flag sets to try, most-loaded first. csh/tcsh reject `-l` combined with
    /// `-c`; fish runs config.fish for every invocation, so `-l` alone loads
    /// everything without interactive-mode side effects.
    fn attempt_flag_sets(shell: &Path) -> Vec<Vec<&'static str>> {
        let name = shell
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        match name.as_str() {
            "csh" | "tcsh" => vec![vec!["-c"]],
            "fish" => vec![vec!["-l", "-c"], vec!["-c"]],
            _ => vec![vec!["-l", "-i", "-c"], vec!["-l", "-c"]],
        }
    }

    /// Run `<shell> <flags> 'echo BEGIN; env; echo END'` per flag set until one
    /// yields a parseable PATH.
    pub(super) fn snapshot_path(shell: &Path, timeout: Duration) -> Option<OsString> {
        let script = format!("echo {BEGIN_MARKER}; env; echo {END_MARKER}");
        for flags in attempt_flag_sets(shell) {
            let output = run_and_capture(shell, &flags, &script, timeout);
            if let Some(path) = parse_snapshot_path(&output) {
                return Some(path);
            }
        }
        None
    }

    /// Spawn the shell and collect stdout until the end marker appears, the
    /// child exits (plus a flush grace), or the timeout kills it. The reader
    /// lives on its own thread appending into a shared buffer, so a shell that
    /// blocks after printing — or a grandchild that inherits the pipe and
    /// never closes it — can't hang us on EOF.
    fn run_and_capture(shell: &Path, flags: &[&str], script: &str, timeout: Duration) -> Vec<u8> {
        let mut cmd = std::process::Command::new(shell);
        cmd.args(flags)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Let rc files detect (and skip work for) this probe, mirroring
            // VSCODE_RESOLVING_ENVIRONMENT; TERM=dumb quiets fancy prompts.
            .env("COMET_RESOLVING_ENVIRONMENT", "1")
            .env("TERM", "dumb");
        let Ok(mut child) = cmd.spawn() else {
            return Vec::new();
        };
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        if let Some(mut stdout) = child.stdout.take() {
            let buf = Arc::clone(&buf);
            let _ = std::thread::Builder::new()
                .name("comet-shell-env-read".into())
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stdout.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut b = buf
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                if b.len() >= MAX_OUTPUT {
                                    break;
                                }
                                b.extend_from_slice(&chunk[..n]);
                            }
                        }
                    }
                });
        }
        let deadline = Instant::now() + timeout;
        let mut exited_at: Option<Instant> = None;
        let mut scanned = 0usize;
        loop {
            {
                let b = buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only scan the unscanned tail (minus marker-length overlap).
                let from = scanned.saturating_sub(END_MARKER.len());
                if find_subslice(&b[from..], END_MARKER.as_bytes()).is_some() {
                    break;
                }
                scanned = b.len();
            }
            match exited_at {
                Some(at) if at.elapsed() >= EXIT_FLUSH_GRACE => break,
                Some(_) => {}
                None => match child.try_wait() {
                    Ok(Some(_)) => exited_at = Some(Instant::now()),
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                },
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if exited_at.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let b = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.clone()
    }

    /// Extract PATH from the `env` dump between the LAST begin marker and the
    /// first end marker after it (rc noise printed before our command — or a
    /// marker echoed by init itself — lands before the real one).
    fn parse_snapshot_path(output: &[u8]) -> Option<OsString> {
        let begin = rfind_subslice(output, BEGIN_MARKER.as_bytes())?;
        let after = &output[begin + BEGIN_MARKER.len()..];
        let end = find_subslice(after, END_MARKER.as_bytes())?;
        for line in after[..end].split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(value) = line.strip_prefix(b"PATH=")
                && !value.is_empty()
            {
                return Some(OsString::from_vec(value.to_vec()));
            }
        }
        None
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .rposition(|window| window == needle)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fake_shell(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("fake-shell");
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// A fake $SHELL skeleton: consume flags, exec the `-c` payload.
        const RUN_PAYLOAD: &str = r#"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-c" ]; then shift; exec /bin/sh -c "$1"; fi
  shift
done
exit 1
"#;

        #[test]
        fn parses_path_between_markers() {
            let output = format!(
                "rc noise\n{BEGIN_MARKER}\nHOME=/home/u\nPATH=/custom/bin:/usr/bin\nX=y\n{END_MARKER}\ntrailing"
            );
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/custom/bin:/usr/bin"));
        }

        #[test]
        fn ignores_marker_echoed_by_init() {
            // rc noise that happens to contain the begin marker but no PATH
            // after it must not shadow the real snapshot.
            let output =
                format!("{BEGIN_MARKER}\ngarbage\n{BEGIN_MARKER}\nPATH=/real/bin\n{END_MARKER}\n");
            let path = parse_snapshot_path(output.as_bytes()).unwrap();
            assert_eq!(path, OsString::from("/real/bin"));
        }

        #[test]
        fn snapshots_path_from_fake_shell() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\nPATH=\"/comet-test/custom/bin:/usr/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let path = snapshot_path(&shell, Duration::from_secs(10)).unwrap();
            let path = path.to_string_lossy();
            assert!(path.starts_with("/comet-test/custom/bin:"), "got: {path}");
        }

        #[test]
        fn falls_back_when_interactive_attempt_hangs() {
            let dir = tempfile::tempdir().unwrap();
            // Simulates rc files that wedge only in interactive mode (`exec
            // tmux` and friends): sleep forever when -i is present.
            let shell = fake_shell(
                dir.path(),
                &format!(
                    "#!/bin/sh\ncase \" $* \" in *\" -i \"*) sleep 60;; esac\nPATH=\"/comet-test/fallback/bin:/bin\"; export PATH\n{RUN_PAYLOAD}"
                ),
            );
            let start = Instant::now();
            let path = snapshot_path(&shell, Duration::from_millis(400)).unwrap();
            assert!(
                path.to_string_lossy()
                    .starts_with("/comet-test/fallback/bin"),
                "got: {}",
                path.to_string_lossy()
            );
            // First attempt burned ~400ms then was killed; the whole resolve
            // must not have waited out the sleep.
            assert!(start.elapsed() < Duration::from_secs(5));
        }

        #[test]
        fn gives_up_on_a_shell_that_never_answers() {
            let dir = tempfile::tempdir().unwrap();
            let shell = fake_shell(dir.path(), "#!/bin/sh\nsleep 60\n");
            let start = Instant::now();
            assert!(snapshot_path(&shell, Duration::from_millis(300)).is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }
}
