//! Single-instance lock — an exclusive OS lock on `{data_dir}/engine.lock`
//! held for the engine's lifetime. Two engines sharing one data dir would race the
//! SQLite snapshots DB and the append-only run journals (WAL + `busy_timeout` guard
//! individual statements, not whole-file ownership), so the second instance must
//! fail fast with a clear error instead of corrupting state.
//!
//! The lock is taken in `EngineCore::assemble_with_identity` BEFORE any store is opened
//! and before the IPC port binds, which also closes the race where a headed app's
//! TCP probe sees no daemon during another instance's startup window.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
use std::path::Path;

use crate::EngineError;

/// Held lock on the data dir. Dropping it (engine shutdown / process exit)
/// releases the advisory lock; a crash releases it too (kernel-owned).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Acquire the exclusive lock, non-blocking. Errors with a descriptive
    /// message (including the holder's pid when readable) if another engine
    /// already owns this data dir.
    pub fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("engine.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Bounded EWOULDBLOCK retries: a fork→exec window in ANY process
            // that inherited the previous holder's fd (git scans, harness
            // spawns — fds are duplicated between fork and CLOEXEC-at-exec)
            // keeps the flock alive for a few milliseconds after release. A
            // real second engine holds it forever; transient artifacts clear
            // well within the budget.
            let mut retries = 40u32; // × 25ms = 1s budget
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                let errno = std::io::Error::last_os_error();
                match errno.raw_os_error() {
                    Some(libc::EINTR) => continue, // signal-interrupted: retry
                    Some(libc::EWOULDBLOCK) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Some(libc::EWOULDBLOCK) => {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        let holder = holder.trim();
                        return Err(EngineError::Other(format!(
                            "another zeron engine is already running on {} (pid {}); \
                             stop it or use a different data dir (ZERON_DATA_DIR)",
                            data_dir.display(),
                            if holder.is_empty() { "unknown" } else { holder },
                        )));
                    }
                    // Anything else (ENOLCK, filesystem without flock, …) is an
                    // environment problem, not a second engine — surface it as-is.
                    _ => return Err(EngineError::Io(errno)),
                }
            }
        }

        #[cfg(windows)]
        {
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    let holder = windows_holder_pid(data_dir);
                    return Err(already_running_error(data_dir, &holder));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(EngineError::Io(error)),
            }
        }

        // Best-effort pid stamp for the contention error message above.
        #[cfg(unix)]
        {
            let mut file = &file;
            let _ = file.set_len(0);
            let _ = write!(file, "{}", std::process::id());
            let _ = file.flush();
        }
        #[cfg(windows)]
        {
            // Windows locks the complete byte range, so another handle cannot read the
            // locked file. Keep diagnostics in an unlocked sidecar; its contents are
            // considered only after the OS lock probe reports contention.
            let _ = std::fs::write(windows_pid_path(data_dir), std::process::id().to_string());
        }
        Ok(Self { _file: file })
    }

    /// Best-effort liveness probe: the pid stamped by the engine currently holding
    /// this data dir's lock, `None` when no engine is running (or the platform
    /// cannot test a lock without taking it). Used by `zeron status` and the
    /// login/logout guards; a single non-blocking try — no retry budget — so a
    /// starting engine's transient fork-window artifacts read as "running", which
    /// is the safe direction for those callers.
    pub fn holder(data_dir: &Path) -> Option<String> {
        let path = data_dir.join("engine.lock");
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .ok()?;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                // We took it: nothing is running. Closing the fd releases it, but
                // unlock explicitly so the window is as small as possible.
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                return None;
            }
            let pid = std::fs::read_to_string(&path).unwrap_or_default();
            let pid = pid.trim();
            Some(if pid.is_empty() {
                "unknown".to_string()
            } else {
                pid.to_string()
            })
        }
        #[cfg(windows)]
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .ok()?;
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                    None
                }
                Err(std::fs::TryLockError::WouldBlock) => Some(windows_holder_pid(data_dir)),
                Err(std::fs::TryLockError::Error(_)) => None,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            None
        }
    }
}

#[cfg(windows)]
fn already_running_error(data_dir: &Path, holder: &str) -> EngineError {
    EngineError::Other(format!(
        "another zeron engine is already running on {} (pid {}); \
         stop it or use a different data dir (ZERON_DATA_DIR)",
        data_dir.display(),
        if holder.is_empty() { "unknown" } else { holder },
    ))
}

#[cfg(windows)]
fn windows_pid_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("engine.lock.pid")
}

#[cfg(windows)]
fn windows_holder_pid(data_dir: &Path) -> String {
    std::fs::read_to_string(windows_pid_path(data_dir))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn holder_probe_reports_pid_without_disturbing_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(InstanceLock::holder(dir.path()), None, "unlocked dir");
        let lock = InstanceLock::acquire(dir.path()).expect("acquire");
        assert_eq!(
            InstanceLock::holder(dir.path()).as_deref(),
            Some(std::process::id().to_string().as_str()),
        );
        // The probe must not have stolen the lock from the holder.
        InstanceLock::acquire(dir.path()).expect_err("still held after probe");
        drop(lock);
        assert_eq!(InstanceLock::holder(dir.path()), None, "released");
    }

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).expect("first acquire");
        let err = InstanceLock::acquire(dir.path()).expect_err("second acquire must fail");
        let msg = err.to_string();
        assert!(msg.contains("already running"), "unexpected error: {msg}");
        assert!(
            msg.contains(&std::process::id().to_string()),
            "holder pid missing from error: {msg}"
        );
        drop(lock);
        InstanceLock::acquire(dir.path()).expect("acquire after release");
    }

    #[cfg(windows)]
    #[test]
    fn subprocess_lock_holder() {
        let Some(dir) = std::env::var_os("ZERON_INSTANCE_LOCK_TEST_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let _lock = InstanceLock::acquire(&dir).expect("child acquire");
        std::fs::write(dir.join("ready"), []).expect("signal ready");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !dir.join("release").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "release signal timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    #[test]
    fn subprocess_contention_reports_pid_and_releases_on_exit() {
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .arg("subprocess_lock_holder")
            .arg("--nocapture")
            .env("ZERON_INSTANCE_LOCK_TEST_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lock holder");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dir.path().join("ready").exists() {
            if let Some(status) = child.try_wait().expect("poll lock holder") {
                panic!("lock holder exited before becoming ready: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ready signal timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let child_pid = child.id().to_string();
        assert_eq!(
            InstanceLock::holder(dir.path()).as_deref(),
            Some(child_pid.as_str())
        );
        let err = InstanceLock::acquire(dir.path()).expect_err("parent acquire must contend");
        assert!(
            err.to_string().contains(&child_pid),
            "holder pid missing: {err}"
        );

        std::fs::write(dir.path().join("release"), []).expect("signal release");
        assert!(child.wait().expect("wait for lock holder").success());
        assert_eq!(
            InstanceLock::holder(dir.path()),
            None,
            "released after child exit"
        );
        InstanceLock::acquire(dir.path()).expect("acquire after child exit");
    }
}
