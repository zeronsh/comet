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
    let mut file = std::fs::File::open(staged)?;
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
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected.trim()),
        "staged update checksum mismatch"
    );
    Ok(())
}

/// Replace the running executable. The library handles Windows image locks and
/// deferred cleanup; the desktop relaunch waits for the old engine to exit.
pub fn apply(staged: &Path, directory: &Path, relaunch: bool) -> anyhow::Result<()> {
    let installed = directory.join("zeron.exe");
    ensure!(
        std::env::current_exe()?.canonicalize()? == installed.canonicalize()?,
        "update must run from its installation"
    );
    verify(staged)?;
    self_replace::self_replace(staged).context("replacing Windows executable")?;
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

/// Called by the newly installed desktop before it opens the engine profile.
pub fn wait_for_exit(pid: u32) -> anyhow::Result<()> {
    ensure!(pid != std::process::id(), "cannot wait for own process");
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

#[cfg(test)]
mod tests {
    use super::*;

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
            socket.read(&mut request).unwrap();
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
