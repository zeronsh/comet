use std::fs;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::screenshot::{AvailableTargets, Screenshot, ScreenshotProxy};
use futures::StreamExt as _;
use futures::channel::mpsc;

use super::{WaylandStatus, atspi};
use crate::appshots::{
    CapabilityState, CaptureError, CaptureTarget, CapturedAppshot, png_dimensions,
};
use crate::attachments;

const SHORTCUT_ID: &str = "capture-appshot";

pub(super) fn start_shortcut(
    tx: mpsc::UnboundedSender<()>,
    status: &'static RwLock<WaylandStatus>,
) {
    thread::Builder::new()
        .name("appshot-wayland-portal".into())
        .spawn(move || {
            futures::executor::block_on(probe_capture(status));
            while !crate::appshots::enabled() {
                thread::sleep(Duration::from_millis(250));
            }
            let result = futures::executor::block_on(run_shortcut(tx, status));
            if let Err(error) = result {
                tracing::warn!(?error, "Wayland global-shortcut portal unavailable");
                if let Ok(mut status) = status.write() {
                    status.shortcut = CapabilityState::SetupRequired;
                }
            }
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
            capture: CapabilityState::UserSelection,
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
) -> Result<(), ashpd::Error> {
    // The caller waits for Appshots to be enabled because binding can display
    // compositor-owned configuration UI.
    let portal = GlobalShortcuts::new().await?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    let shortcut = NewShortcut::new(SHORTCUT_ID, "Capture an Appshot")
        .preferred_trigger(Some("CTRL+ALT+SPACE"));
    portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    if let Ok(mut status) = status.write() {
        status.shortcut = CapabilityState::Ready;
    }
    let mut activations = portal.receive_activated().await?;
    while let Some(activation) = activations.next().await {
        if activation.shortcut_id() == SHORTCUT_ID && tx.unbounded_send(()).is_err() {
            break;
        }
    }
    Ok(())
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
    // Active-window targeting and semantic lookup refer to the same focused
    // app. For a portal picker, do not risk pairing one chosen window with
    // another app's accessibility tree.
    let semantics = if target == CaptureTarget::ActiveWindow {
        atspi::capture_focused().await.ok()
    } else {
        None
    };
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
        .map_err(|error| CaptureError::CaptureFailed(format!("Screenshot portal failed: {error}")))?
        .response()
        .map_err(|error| {
            CaptureError::CaptureFailed(format!("Screenshot portal request was cancelled: {error}"))
        })?;
    let uri = url::Url::parse(response.uri().as_str()).map_err(|error| {
        CaptureError::CaptureFailed(format!("Invalid portal image URI: {error}"))
    })?;
    let path = uri.to_file_path().map_err(|_| {
        CaptureError::CaptureFailed("Screenshot portal returned a non-file URI.".into())
    })?;
    let bytes = fs::read(path).map_err(|error| {
        CaptureError::CaptureFailed(format!("Could not read portal screenshot: {error}"))
    })?;
    let dimensions = png_dimensions(&bytes);
    let (app_name, window_title, accessibility) = semantics
        .map(|value| (value.app_name, value.window_title, value.snapshot))
        .unwrap_or_else(|| {
            (
                "Selected window".into(),
                None,
                crate::appshots::AccessibilitySnapshot::unavailable(),
            )
        });
    let screenshot = attachments::stage_png_bytes(format!("{app_name} Appshot.png"), bytes);
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
        screenshot_dimensions: dimensions,
        app_icon: None,
        captured_at: chrono::Utc::now(),
    })
}
