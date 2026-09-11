//! Updates for portable Windows packages. Source builds remain unmanaged.
use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const CONFIG: &str = "zeron-update.json";
/// The running image, moved aside during a swap. A running executable can be
/// renamed but not deleted, so the file survives until the process exits and
/// is removed by the relaunched instance (or the next update attempt).
const BACKUP: &str = "zeron.exe.old";
/// Deterministic name for the copy that becomes the next installation; a
/// crash between the two renames leaves at most this file behind.
const INCOMING: &str = ".zeron-update-incoming.exe";

#[derive(serde::Deserialize)]
struct Config {
    releases_url: String,
}

pub(super) fn is_managed(exe: &Path) -> bool {
    exe.file_name().is_some_and(|name| name == "zeron.exe")
        && exe.parent().is_some_and(|dir| dir.join(CONFIG).is_file())
}

pub(super) fn release_url() -> anyhow::Result<Option<String>> {
    let exe = std::env::current_exe()?;
    if !is_managed(&exe) {
        return Ok(None);
    }
    let config: Config = serde_json::from_slice(&std::fs::read(exe.with_file_name(CONFIG))?)
        .context("reading Windows update configuration")?;
    ensure!(
        config.releases_url.starts_with("https://"),
        "update feed must use HTTPS"
    );
    Ok(Some(config.releases_url))
}

pub fn artifact(version: &str) -> String {
    format!("zeron-{version}-windows-{}.exe", std::env::consts::ARCH)
}

/// Download next to the installation, verifying its mandatory checksum.
/// Each attempt has its own directory, so failed or concurrent downloads cannot
/// leave a reusable, partially staged executable.
pub async fn stage(
    edge_url: &str,
    manifest: &super::Manifest,
    directory: &Path,
) -> anyhow::Result<PathBuf> {
    ensure!(
        manifest
            .version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
        "invalid Windows release version"
    );
    let file = artifact(&manifest.version);
    let expected = manifest
        .files
        .get(&file)
        .and_then(|meta| meta.sha256.as_deref())
        .context("Windows updates require a SHA-256 checksum")?;
    ensure!(
        expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256 checksum"
    );
    let temporary = tempfile::Builder::new()
        .prefix(".zeron-update-")
        .tempdir_in(directory)?;
    let staged = temporary.path().join("zeron.exe");
    super::download_release_file(edge_url, manifest, &file, &staged).await?;
    std::fs::write(temporary.path().join("sha256"), expected)?;
    verify(&staged)?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(&staged)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("staged executable version check timed out")??;
    ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim()
                == format!("zeron {}", manifest.version),
        "staged executable has the wrong version or cannot run"
    );
    let _ = temporary.keep();
    Ok(staged)
}

fn verify(staged: &Path) -> anyhow::Result<()> {
    let expected = std::fs::read_to_string(staged.with_file_name("sha256"))?;
    verify_digest(staged, expected.trim())
}

fn verify_digest(path: &Path, expected: &str) -> anyhow::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected),
        "staged update checksum mismatch"
    );
    Ok(())
}

/// Replace the running executable with the staged update.
///
/// A hand-rolled swap replaces `self-replace` here: that crate renames the
/// running executable away and schedules its deletion *before* copying the
/// replacement back, so a failed copy (disk full, antivirus lock) left no
/// launchable installation. This swap copies first, moves the running image
/// aside only when the replacement is verified in place, and restores the
/// previous executable on every failure after that move. The relaunch cleanup
/// and [`Self::recover_from_backup`] cover the leftovers of a hard crash
/// between the two renames.
pub fn apply(staged: &Path, directory: &Path, relaunch: bool) -> anyhow::Result<()> {
    let installed = directory.join("zeron.exe");
    ensure!(
        std::env::current_exe()?.canonicalize()? == installed.canonicalize()?,
        "update must run from its installation"
    );
    verify(staged)?;
    recover_from_backup(&installed)?;
    swap_into_place(staged, &installed)?;
    if relaunch {
        std::process::Command::new(&installed)
            .args(["--wait-for-exit", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("relaunching updated application")?;
    }
    let _ = std::fs::remove_file(staged);
    let _ = std::fs::remove_file(staged.with_file_name("sha256"));
    if let Some(parent) = staged.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// Settle leftovers from a previous swap: restore the backup when the
/// installation itself is missing (a crash between the two renames), or drop
/// the backup when the installation is already in place.
fn recover_from_backup(installed: &Path) -> anyhow::Result<()> {
    let Some(directory) = installed.parent() else {
        return Ok(());
    };
    let backup = directory.join(BACKUP);
    if !backup.exists() {
        return Ok(());
    }
    if installed.exists() {
        let _ = std::fs::remove_file(&backup);
        return Ok(());
    }
    std::fs::rename(&backup, installed)
        .context("restoring the installation from its pre-update backup")
}

/// Move `staged` onto `installed` through a verified copy, keeping the old
/// image as a recoverable backup until the new one is in place.
fn swap_into_place(staged: &Path, installed: &Path) -> anyhow::Result<()> {
    let directory = installed
        .parent()
        .context("installation has no parent directory")?;
    let backup = directory.join(BACKUP);
    let incoming = directory.join(INCOMING);
    let expected = std::fs::read_to_string(staged.with_file_name("sha256"))
        .context("reading the staged checksum")?;
    let expected = expected.trim().to_owned();

    // Copy and re-verify before touching the installation: an error here, or
    // a corrupt copy, leaves the launch path alone.
    if let Err(err) = std::fs::copy(staged, &incoming)
        .and_then(|_| verify_digest(&incoming, &expected).map_err(std::io::Error::other))
    {
        let _ = std::fs::remove_file(&incoming);
        return Err(err).context("copying the staged update into the installation directory");
    }

    // The running image can be renamed but not deleted; move it aside only
    // now that the replacement is verified next to it.
    if let Err(err) = std::fs::rename(installed, &backup) {
        let _ = std::fs::remove_file(&incoming);
        return Err(err).context("moving the running executable aside for replacement");
    }
    if let Err(err) = std::fs::rename(&incoming, installed) {
        // The installation must not be left missing: restore the backup or
        // fail loudly about both errors.
        std::fs::rename(&backup, installed)
            .context("restoring the previous executable after a failed update")?;
        return Err(err).context("installing the updated executable");
    }
    Ok(())
}

/// Called by the newly installed desktop before it opens the engine profile.
/// The old image, renamed aside by the update, can only be deleted after its
/// process exits — this is that moment.
pub fn wait_for_exit(pid: u32) -> anyhow::Result<()> {
    ensure!(pid != std::process::id(), "cannot wait for own process");
    let result = wait_for_exit_impl(pid);
    let _ = remove_own_backup();
    result
}

fn wait_for_exit_impl(pid: u32) -> anyhow::Result<()> {
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(()); // The previous instance already exited.
        }
        return Err(error).context("opening previous instance");
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    if unsafe { WaitForSingleObject(process.as_raw_handle(), 60000) } != WAIT_OBJECT_0 {
        bail!("previous instance did not exit within 60 seconds");
    }
    Ok(())
}

fn remove_own_backup() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let Some(directory) = exe.parent() else {
        return Ok(());
    };
    std::fs::remove_file(directory.join(BACKUP))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open handle sharing read/write but NOT delete: `std::fs::copy` can
    /// still write through it, while the final rename (which needs delete
    /// access) fails with a sharing violation — a deterministic stand-in for
    /// antivirus or file-sharing errors after the old image has moved.
    struct NoDeleteHandle(*mut core::ffi::c_void);
    impl NoDeleteHandle {
        fn open(path: &Path) -> Self {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Foundation::GENERIC_READ;
            use windows_sys::Win32::Storage::FileSystem::{
                CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            };
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            let raw = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert!(
                raw != -1isize as _,
                "CreateFileW failed: {}",
                std::io::Error::last_os_error()
            );
            Self(raw)
        }
    }
    impl Drop for NoDeleteHandle {
        fn drop(&mut self) {
            use windows_sys::Win32::Foundation::CloseHandle;
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A staged update layout: `<dir>/zeron.exe` plus its `sha256` sidecar.
    fn staged_with(dir: &Path, bytes: &[u8]) -> PathBuf {
        let staged = dir.join("zeron.exe");
        std::fs::write(&staged, bytes).unwrap();
        std::fs::write(dir.join("sha256"), format!("{:x}", Sha256::digest(bytes))).unwrap();
        staged
    }

    #[test]
    fn portable_install_requires_explicit_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("zeron.exe");
        assert!(!is_managed(&exe));
        std::fs::write(dir.path().join(CONFIG), "{}").unwrap();
        assert!(is_managed(&exe));
        assert!(!is_managed(&dir.path().join("another.exe")));
    }

    #[test]
    fn changed_staging_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("zeron.exe");
        std::fs::write(&exe, b"original").unwrap();
        std::fs::write(
            dir.path().join("sha256"),
            format!("{:x}", Sha256::digest(b"original")),
        )
        .unwrap();
        verify(&exe).unwrap();
        std::fs::write(&exe, b"changed").unwrap();
        assert!(verify(&exe).is_err());
    }

    #[test]
    fn failed_swap_after_old_image_moved_restores_the_installation() {
        let install = tempfile::tempdir().unwrap();
        let installed = install.path().join("zeron.exe");
        std::fs::write(&installed, b"old image").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let staged = staged_with(stage.path(), b"new image");

        // The incoming file exists (so the handle can bind to it) and is held
        // open without delete sharing: the copy succeeds, the swap's final
        // rename fails after the running image has already been moved aside.
        let incoming = install.path().join(INCOMING);
        std::fs::write(&incoming, b"placeholder").unwrap();
        let lock = NoDeleteHandle::open(&incoming);

        let error = swap_into_place(&staged, &installed).unwrap_err();
        drop(lock);
        assert!(
            error
                .to_string()
                .contains("installing the updated executable"),
            "unexpected error: {error:#}"
        );
        // The launchable installation is the old executable again, and no
        // backup is left dangling.
        assert_eq!(std::fs::read(&installed).unwrap(), b"old image");
        assert!(!install.path().join(BACKUP).exists());

        // With the lock released the same swap succeeds end to end.
        swap_into_place(&staged, &installed).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"new image");
        assert!(!incoming.exists());
        assert!(install.path().join(BACKUP).exists());
    }

    #[test]
    fn corrupt_copy_is_rejected_before_the_installation_moves() {
        let install = tempfile::tempdir().unwrap();
        let installed = install.path().join("zeron.exe");
        std::fs::write(&installed, b"old image").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let staged = staged_with(stage.path(), b"new image");
        // Corrupt the staged bytes after the sidecar was written.
        std::fs::write(&staged, b"tampered").unwrap();

        let error = swap_into_place(&staged, &installed).unwrap_err();
        assert!(
            error.to_string().contains("copying the staged update"),
            "unexpected error: {error:#}"
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"old image");
        assert!(!install.path().join(BACKUP).exists());
    }

    #[test]
    fn backup_recovers_a_missing_installation_and_clears_when_intact() {
        let install = tempfile::tempdir().unwrap();
        let installed = install.path().join("zeron.exe");
        let backup = install.path().join(BACKUP);
        std::fs::write(&backup, b"survivor").unwrap();

        // Installation missing: the backup is promoted back into place.
        recover_from_backup(&installed).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"survivor");
        assert!(!backup.exists());

        // Installation present: a stale backup is dropped, not applied.
        std::fs::write(&backup, b"stale").unwrap();
        recover_from_backup(&installed).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), b"survivor");
        assert!(!backup.exists());
    }

    #[tokio::test]
    async fn missing_checksum_is_rejected_before_download() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = super::super::Manifest {
            version: "1.2.3".into(),
            ..Default::default()
        };
        assert!(
            stage("http://127.0.0.1:1", &manifest, dir.path())
                .await
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn corrupt_download_preserves_install_and_removes_staging() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt",
                )
                .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("zeron.exe");
        std::fs::write(&installed, b"existing installation").unwrap();
        let manifest = super::super::Manifest {
            version: "1.2.3".into(),
            files: [(
                artifact("1.2.3"),
                super::super::FileMeta {
                    sha256: Some(format!("{:x}", Sha256::digest(b"expected"))),
                },
            )]
            .into(),
        };
        let error = stage(&base, &manifest, dir.path()).await.unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(std::fs::read(&installed).unwrap(), b"existing installation");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
