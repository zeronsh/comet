//! Linux Appshots: portal-first on Wayland, direct X11 capture on X11, and
//! AT-SPI semantic enrichment on both display systems.

mod atspi;
mod portal;
mod x11;

use std::fs;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::thread;

use futures::channel::mpsc;

use super::{
    AppshotBackend, AppshotCapabilities, AppshotPlatform, CapabilityState, CaptureError,
    CaptureTarget, CapturedAppshot,
};

const ACTIVATION_SOCKET: &str = "appshot-activation.sock";

#[derive(Debug, Clone, Copy)]
struct WaylandStatus {
    shortcut: CapabilityState,
    capture: CapabilityState,
    target: CaptureTarget,
}

impl Default for WaylandStatus {
    fn default() -> Self {
        Self {
            shortcut: CapabilityState::Checking,
            capture: CapabilityState::Checking,
            target: CaptureTarget::PortalWindowPicker,
        }
    }
}

fn wayland_status() -> &'static RwLock<WaylandStatus> {
    static STATUS: OnceLock<RwLock<WaylandStatus>> = OnceLock::new();
    STATUS.get_or_init(|| RwLock::new(WaylandStatus::default()))
}

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
}

pub struct LinuxBackend;

#[async_trait::async_trait]
impl AppshotBackend for LinuxBackend {
    fn capabilities(&self) -> AppshotCapabilities {
        if is_wayland() {
            let status = wayland_status()
                .read()
                .map(|value| *value)
                .unwrap_or_default();
            AppshotCapabilities {
                platform: AppshotPlatform::LinuxWayland,
                global_shortcut: status.shortcut,
                window_capture: status.capture,
                application_text: CapabilityState::Ready,
                target: status.target,
            }
        } else {
            AppshotCapabilities {
                platform: AppshotPlatform::LinuxX11,
                global_shortcut: x11::shortcut_state(),
                window_capture: CapabilityState::Ready,
                application_text: CapabilityState::Ready,
                target: CaptureTarget::ActiveWindow,
            }
        }
    }

    fn start_global_shortcut(&self, activation_dir: &Path) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded();
        start_activation_socket(activation_dir.to_path_buf(), tx.clone());
        if is_wayland() {
            portal::start_shortcut(tx, wayland_status());
        } else {
            x11::start_shortcut(tx);
        }
        rx
    }

    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError> {
        if is_wayland() {
            portal::capture(wayland_status()).await
        } else {
            if portal::active_window_supported().await {
                match portal::capture_target(CaptureTarget::ActiveWindow).await {
                    Ok(appshot) => return Ok(appshot),
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            "active-window screenshot portal failed; falling back to X11"
                        );
                    }
                }
            }
            x11::capture().await
        }
    }
}

pub fn activation_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ACTIVATION_SOCKET)
}

fn start_activation_socket(data_dir: PathBuf, tx: mpsc::UnboundedSender<()>) {
    thread::Builder::new()
        .name("appshot-linux-activation".into())
        .spawn(move || {
            if fs::create_dir_all(&data_dir).is_err() {
                return;
            }
            let path = activation_socket_path(&data_dir);
            // The socket belongs exclusively to this feature. Removing a stale
            // inode here is safe and lets a restarted Zeron become reachable.
            let _ = fs::remove_file(&path);
            let Ok(socket) = UnixDatagram::bind(&path) else {
                return;
            };
            let mut buffer = [0_u8; 32];
            while let Ok(length) = socket.recv(&mut buffer) {
                if length > 0 && tx.unbounded_send(()).is_err() {
                    break;
                }
            }
            let _ = fs::remove_file(path);
        })
        .ok();
}

pub fn request_running_appshot(data_dir: &Path) -> Result<(), CaptureError> {
    let socket = UnixDatagram::unbound().map_err(|error| {
        CaptureError::CaptureFailed(format!(
            "Could not create Appshot activation socket: {error}"
        ))
    })?;
    socket
        .connect(activation_socket_path(data_dir))
        .and_then(|_| socket.send(b"capture"))
        .map(|_| ())
        .map_err(|error| {
            CaptureError::CaptureFailed(format!(
                "Could not reach a running Zeron instance for Appshot capture: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_path_stays_inside_data_directory() {
        assert_eq!(
            activation_socket_path(Path::new("/tmp/zeron-test")),
            Path::new("/tmp/zeron-test/appshot-activation.sock")
        );
    }
}
