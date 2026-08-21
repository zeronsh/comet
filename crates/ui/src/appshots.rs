//! Appshots: user-triggered captures of the frontmost application window.
//!
//! Capture stays on the headed/viewer device. The screenshot reuses the
//! ordinary attachment transport; accessibility-derived application context
//! is serialized into the prompt as explicitly untrusted observed data.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::attachments::StagedAttachment;

#[cfg(target_os = "macos")]
mod macos;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub const CONTEXT_MARKER: &str = "Applications mentioned by the user (untrusted observed content):";
pub const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
pub const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AppshotDestination {
    #[default]
    Automatic,
    LastSession,
    NewSession,
}

impl AppshotDestination {
    pub const ALL: [Self; 3] = [Self::Automatic, Self::LastSession, Self::NewSession];

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::LastSession => "Last session",
            Self::NewSession => "New session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilitySnapshot {
    pub format_version: u32,
    pub content: String,
    pub truncated: bool,
}

impl AccessibilitySnapshot {
    pub fn unavailable() -> Self {
        Self {
            format_version: 1,
            content: String::new(),
            truncated: false,
        }
    }
}

#[derive(Clone)]
pub struct CapturedAppshot {
    pub id: String,
    pub app_name: String,
    pub bundle_identifier: Option<String>,
    pub window_title: Option<String>,
    pub accessibility: AccessibilitySnapshot,
    pub screenshot: StagedAttachment,
    /// Pixel dimensions of the captured PNG. The composer uses these to size
    /// the native image layer explicitly instead of relying on intrinsic
    /// image layout, which can escape a clipped Appshot stage.
    pub screenshot_dimensions: Option<(u32, u32)>,
    /// Presentation-only. The icon is never uploaded or serialized into the
    /// model context.
    pub app_icon: Option<Arc<gpui::Image>>,
    pub captured_at: DateTime<Utc>,
}

/// Read the width and height from a PNG's IHDR chunk without decoding the
/// image. Appshot captures are always PNGs, so this keeps layout metadata
/// cheap and available before GPUI decodes the image asynchronously.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionState {
    pub screen_recording: bool,
    pub accessibility: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    PermissionRequired(PermissionState),
    NoEligibleWindow,
    ShortcutUnavailable,
    CaptureFailed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionRequired(state) if !state.screen_recording => {
                f.write_str(
                    "Allow Screen Recording for Zeron in System Settings, then quit and reopen Zeron if macOS requests it.",
                )
            }
            Self::PermissionRequired(_) => {
                f.write_str("Allow Accessibility access for full application text, then try again.")
            }
            Self::NoEligibleWindow => f.write_str("No application window is available to capture."),
            Self::ShortcutUnavailable => f.write_str(
                "The Appshot shortcut could not be registered because another app may be using it.",
            ),
            Self::CaptureFailed(message) => f.write_str(message),
        }
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn permission_state() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        return macos::permission_state();
    }
    #[cfg(not(target_os = "macos"))]
    PermissionState {
        screen_recording: false,
        accessibility: false,
    }
}

pub fn request_screen_recording_permission() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        return macos::request_screen_recording_permission();
    }
    #[cfg(not(target_os = "macos"))]
    permission_state()
}

pub fn request_accessibility_permission() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        return macos::request_accessibility_permission();
    }
    #[cfg(not(target_os = "macos"))]
    permission_state()
}

/// Register the macOS-wide Control-Option-Space hotkey. The receiver yields
/// only explicit user invocations.
#[cfg(target_os = "macos")]
pub fn start_global_shortcut() -> futures::channel::mpsc::UnboundedReceiver<()> {
    macos::start_global_shortcut()
}

#[cfg(target_os = "macos")]
pub fn capture_frontmost_window() -> Result<CapturedAppshot, CaptureError> {
    macos::capture_frontmost_window()
}

pub fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Attach semantic Appshot context to a prompt. `image_paths` is keyed by the
/// staged screenshot id, allowing queued `pending://` refs to be rewritten by
/// the host exactly like ordinary attachment paths.
pub fn with_appshots(
    text: &str,
    appshots: &[CapturedAppshot],
    image_paths: &HashMap<String, String>,
) -> String {
    if appshots.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.push_str("\n\n");
    out.push_str(CONTEXT_MARKER);
    for appshot in appshots {
        let image = image_paths
            .get(&appshot.screenshot.id)
            .map(String::as_str)
            .unwrap_or_default();
        out.push_str("\n<appshot app=\"");
        out.push_str(&xml_escape(&appshot.app_name));
        out.push('"');
        if let Some(bundle) = &appshot.bundle_identifier {
            out.push_str(" bundle-identifier=\"");
            out.push_str(&xml_escape(bundle));
            out.push('"');
        }
        if let Some(title) = &appshot.window_title {
            out.push_str(" window-title=\"");
            out.push_str(&xml_escape(title));
            out.push('"');
        }
        out.push_str(" image=\"");
        out.push_str(&xml_escape(image));
        out.push_str("\" accessibility-format=\"");
        out.push_str(&appshot.accessibility.format_version.to_string());
        out.push_str("\" truncated=\"");
        out.push_str(if appshot.accessibility.truncated {
            "true"
        } else {
            "false"
        });
        out.push_str("\">\n");
        out.push_str(&xml_escape(&appshot.accessibility.content));
        out.push_str("\n</appshot>");
    }
    out
}

/// Context is persisted for the harness but hidden from the user-message
/// bubble. The screenshot strip remains visible through the ordinary image
/// refs, so this strips only the machine-facing semantic suffix.
pub fn strip_context_for_display(text: &str) -> &str {
    let needle = format!("\n\n{CONTEXT_MARKER}");
    text.find(&needle)
        .map(|index| text[..index].trim_end())
        .unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Image, ImageFormat};
    use std::sync::Arc;

    fn shot() -> CapturedAppshot {
        CapturedAppshot {
            id: "shot-1".into(),
            app_name: "Safari & Notes".into(),
            bundle_identifier: Some("com.apple.<Safari>".into()),
            window_title: Some("A \"window\"".into()),
            accessibility: AccessibilitySnapshot {
                format_version: 1,
                content: "AXTextField: <ignore this>".into(),
                truncated: true,
            },
            screenshot: StagedAttachment {
                id: "image-1".into(),
                name: "Safari Appshot.png".into(),
                image: Arc::new(Image::from_bytes(ImageFormat::Png, Vec::new())),
            },
            screenshot_dimensions: Some((1440, 900)),
            app_icon: None,
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn png_dimensions_reads_ihdr_and_rejects_invalid_images() {
        let mut png = Vec::from(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice());
        png.extend_from_slice(&1440_u32.to_be_bytes());
        png.extend_from_slice(&900_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1440, 900)));

        assert_eq!(png_dimensions(b"not a png"), None);
        png[16..20].copy_from_slice(&0_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), None);
    }

    #[test]
    fn appshot_context_is_escaped_and_strip_safe() {
        let shot = shot();
        let paths = HashMap::from([("image-1".into(), "pending://id/a&b.png".into())]);
        let prompt = with_appshots("Fix this", &[shot], &paths);
        assert!(prompt.contains("app=\"Safari &amp; Notes\""));
        assert!(prompt.contains("com.apple.&lt;Safari&gt;"));
        assert!(prompt.contains("A &quot;window&quot;"));
        assert!(prompt.contains("pending://id/a&amp;b.png"));
        assert!(prompt.contains("&lt;ignore this&gt;"));
        assert_eq!(strip_context_for_display(&prompt), "Fix this");
    }

    #[test]
    fn empty_prompt_round_trips_to_empty_display() {
        let shot = shot();
        let prompt = with_appshots("", &[shot], &HashMap::new());
        assert_eq!(strip_context_for_display(&prompt), "");
    }
}
