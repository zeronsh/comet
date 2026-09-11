use std::fs;
use std::io::Read as _;
use std::sync::RwLock;
use std::thread;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::screenshot::{AvailableTargets, Screenshot, ScreenshotProxy};
use futures::channel::mpsc;
use futures::{FutureExt as _, StreamExt as _};

use super::WaylandStatus;
use crate::appshots::{CapabilityState, CaptureError, CaptureTarget, CapturedAppshot};

const SHORTCUT_ID: &str = "capture-appshot";

pub(super) fn start_shortcut(
    tx: mpsc::UnboundedSender<()>,
    status: &'static RwLock<WaylandStatus>,
) {
    thread::Builder::new()
        .name("appshot-wayland-portal".into())
        .spawn(move || {
            futures::executor::block_on(probe_capture(status));
            futures::executor::block_on(async {
                let mut updates = crate::appshots::shortcut::subscribe();
                let mut desired = updates.next().await.flatten();
                let mut last_bound = None;
                loop {
                    let Some(shortcut) = desired.take() else {
                        let Some(next) = updates.next().await else {
                            break;
                        };
                        desired = next;
                        continue;
                    };
                    let configure = last_bound.as_ref().is_some_and(|old| old != &shortcut);
                    match run_shortcut(tx.clone(), status, &shortcut, configure, &mut updates).await
                    {
                        Ok(next) => {
                            last_bound = Some(shortcut);
                            desired = next;
                        }
                        Err(error) => {
                            tracing::warn!(?error, "Wayland global-shortcut portal unavailable");
                            if let Ok(mut status) = status.write() {
                                status.shortcut = CapabilityState::SetupRequired;
                            }
                            let Some(next) = updates.next().await else {
                                break;
                            };
                            desired = next;
                        }
                    }
                }
            });
        })
        .ok();
}

async fn probe_capture(status: &'static RwLock<WaylandStatus>) {
    let detected = match ScreenshotProxy::new().await {
        Ok(proxy) if proxy.version() >= 3 => match proxy.available_targets().await {
            Ok(targets) if targets.contains(AvailableTargets::ActiveWindow) => WaylandStatus {
                capture: CapabilityState::Ready,
                target: CaptureTarget::ActiveWindow,
                ..status.read().map(|value| *value).unwrap_or_default()
            },
            Ok(targets) if targets.contains(AvailableTargets::Window) => WaylandStatus {
                capture: CapabilityState::UserSelection,
                target: CaptureTarget::PortalWindowPicker,
                ..status.read().map(|value| *value).unwrap_or_default()
            },
            _ => WaylandStatus {
                capture: CapabilityState::Unavailable,
                ..status.read().map(|value| *value).unwrap_or_default()
            },
        },
        Ok(_) => WaylandStatus {
            capture: CapabilityState::Unavailable,
            target: CaptureTarget::PortalWindowPicker,
            ..status.read().map(|value| *value).unwrap_or_default()
        },
        Err(_) => WaylandStatus {
            capture: CapabilityState::Unavailable,
            ..status.read().map(|value| *value).unwrap_or_default()
        },
    };
    if let Ok(mut current) = status.write() {
        current.capture = detected.capture;
        current.target = detected.target;
    }
}

pub(super) async fn active_window_supported() -> bool {
    match ScreenshotProxy::new().await {
        Ok(proxy) if proxy.version() >= 3 => proxy
            .available_targets()
            .await
            .is_ok_and(|targets| targets.contains(AvailableTargets::ActiveWindow)),
        _ => false,
    }
}

async fn run_shortcut(
    tx: mpsc::UnboundedSender<()>,
    status: &'static RwLock<WaylandStatus>,
    selected: &crate::appshots::shortcut::Shortcut,
    configure: bool,
    updates: &mut mpsc::UnboundedReceiver<Option<crate::appshots::shortcut::Shortcut>>,
) -> Result<Option<crate::appshots::shortcut::Shortcut>, ashpd::Error> {
    let portal = GlobalShortcuts::new().await?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    // Always close the old session before rebinding, including when consent is
    // denied or the feature is switched off. Dropping the proxy does not close it.
    let result = async {
        let trigger = selected.portal_trigger();
        let shortcut = NewShortcut::new(SHORTCUT_ID, "Capture an Appshot").preferred_trigger(Some(trigger.as_str()));
        portal.bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default()).await?.response()?;
        if configure && portal.version() >= 2 {
            portal.configure_shortcuts(&session, None, ConfigureShortcutsOptions::default()).await?;
        }
        if let Ok(mut status) = status.write() { status.shortcut = CapabilityState::Ready; }
        let mut activations = portal.receive_activated().await?;
        loop {
            futures::select! {
                next = updates.next().fuse() => {
                    let next = next.flatten();
                    if next.as_ref() != Some(selected) { return Ok(next); }
                },
                activation = activations.next().fuse() => {
                    let Some(activation) = activation else { return Ok(None); };
                    if activation.shortcut_id() == SHORTCUT_ID && tx.unbounded_send(()).is_err() { return Ok(None); }
                },
            }
        }
    }.await;
    let _ = session.close().await;
    result
}

pub(super) async fn capture(
    status: &'static RwLock<WaylandStatus>,
) -> Result<CapturedAppshot, CaptureError> {
    probe_capture(status).await;
    let target = status
        .read()
        .map(|value| value.target)
        .unwrap_or(CaptureTarget::PortalWindowPicker);
    capture_target(target).await
}

pub(super) async fn capture_target(target: CaptureTarget) -> Result<CapturedAppshot, CaptureError> {
    // Do not send an unsupported target: older portals may silently capture
    // the entire screen, even when interactive customization is requested.
    let proxy = ScreenshotProxy::new().await.map_err(|error| {
        CaptureError::CaptureFailed(format!("Screenshot portal unavailable: {error}"))
    })?;
    let requested = match target {
        CaptureTarget::ActiveWindow => AvailableTargets::ActiveWindow,
        CaptureTarget::PortalWindowPicker => AvailableTargets::Window,
    };
    if proxy.version() < 3
        || !proxy
            .available_targets()
            .await
            .is_ok_and(|targets| targets.contains(requested))
    {
        return Err(CaptureError::CaptureFailed(
            "This screenshot portal does not support window-only capture. Update your desktop portal to use Appshots.".into(),
        ));
    }
    // The Screenshot portal returns pixels but no verifiable native window
    // identity. Never attach text from whichever window happens to have focus.
    let portal_target = match target {
        CaptureTarget::ActiveWindow => AvailableTargets::ActiveWindow,
        CaptureTarget::PortalWindowPicker => AvailableTargets::Window,
    };
    let response = Screenshot::request()
        .target(portal_target)
        .interactive(target == CaptureTarget::PortalWindowPicker)
        .modal(false)
        .send()
        .await
        .map_err(portal_capture_error)?
        .response()
        .map_err(portal_capture_error)?;
    let uri = url::Url::parse(response.uri().as_str()).map_err(|error| {
        CaptureError::CaptureFailed(format!("Invalid portal image URI: {error}"))
    })?;
    let path = uri.to_file_path().map_err(|_| {
        CaptureError::CaptureFailed("Screenshot portal returned a non-file URI.".into())
    })?;
    let file_len = fs::metadata(&path)
        .map_err(|error| {
            CaptureError::CaptureFailed(format!("Could not inspect portal screenshot: {error}"))
        })?
        .len();
    if file_len > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(CaptureError::CaptureFailed(
            "The portal screenshot is larger than Zeron's 24 MB image limit.".into(),
        ));
    }
    let file = fs::File::open(path).map_err(|error| {
        CaptureError::CaptureFailed(format!("Could not open portal screenshot: {error}"))
    })?;
    let mut bytes = Vec::with_capacity(file_len as usize);
    file.take(crate::attachments::MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CaptureError::CaptureFailed(format!("Could not read portal screenshot: {error}"))
        })?;
    if bytes.len() as u64 > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(CaptureError::CaptureFailed(
            "The portal screenshot changed size while it was being read.".into(),
        ));
    }
    let app_name = "Selected window".to_string();
    let window_title = None;
    let accessibility = crate::appshots::AccessibilitySnapshot::unavailable();
    let (screenshot, dimensions) = super::super::stage_appshot_png(&app_name, bytes)?;
    Ok(CapturedAppshot {
        id: uuid::Uuid::new_v4().to_string(),
        app_name,
        bundle_identifier: Some(
            match target {
                CaptureTarget::ActiveWindow => "linux-portal:active-window",
                CaptureTarget::PortalWindowPicker => "linux-portal:selection",
            }
            .into(),
        ),
        window_title,
        accessibility,
        screenshot,
        screenshot_dimensions: Some(dimensions),
        app_icon: None,
        captured_at: chrono::Utc::now(),
    })
}

fn portal_capture_error(error: ashpd::Error) -> CaptureError {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
        | ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => CaptureError::Cancelled,
        error => CaptureError::CaptureFailed(format!("Screenshot portal failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_survives_both_portal_error_paths() {
        assert_eq!(
            portal_capture_error(ashpd::Error::Response(
                ashpd::desktop::ResponseError::Cancelled
            )),
            CaptureError::Cancelled
        );
        assert_eq!(
            portal_capture_error(ashpd::Error::Portal(ashpd::PortalError::Cancelled(
                "cancelled".into()
            ))),
            CaptureError::Cancelled
        );
        assert!(matches!(
            portal_capture_error(ashpd::Error::NoResponse),
            CaptureError::CaptureFailed(_)
        ));
    }
}
