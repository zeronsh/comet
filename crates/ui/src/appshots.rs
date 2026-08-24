//! Appshots: user-triggered captures of the frontmost application window.
//!
//! Capture stays on the headed/viewer device. The screenshot reuses the
//! ordinary attachment transport; accessibility-derived application context
//! is serialized into the prompt as explicitly untrusted observed data.

use std::collections::HashMap;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::attachments::StagedAttachment;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Bounds native capture buffers before platform APIs allocate or stage them.
pub const MAX_CAPTURE_DIMENSION: u32 = 8_192;
pub const MAX_CAPTURE_PIXELS: u64 = 32 * 1024 * 1024;
pub const MAX_CAPTURE_RGBA_BYTES: u64 = 128 * 1024 * 1024;
/// A composer may retain several captures while the user prepares a prompt,
/// but it must not become an unbounded store of decoded image data.
pub const MAX_STAGED_APPSHOT_BYTES: u64 = 4 * crate::attachments::MAX_ATTACHMENT_BYTES;

pub const CONTEXT_MARKER: &str = "Applications mentioned by the user (untrusted observed content):";
#[cfg(target_os = "macos")]
const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
#[cfg(target_os = "macos")]
const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppshotPlatform {
    MacOs,
    Windows,
    LinuxWayland,
    LinuxX11,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityState {
    Checking,
    Ready,
    PermissionRequired,
    SetupRequired,
    UserSelection,
    Unavailable,
}

impl CapabilityState {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Checking => "Checking",
            Self::Ready => "Ready",
            Self::PermissionRequired => "Required",
            Self::SetupRequired => "Set up",
            Self::UserSelection => "Select window",
            Self::Unavailable => "Unavailable",
        }
    }

    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready | Self::UserSelection)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    ActiveWindow,
    PortalWindowPicker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppshotCapabilities {
    pub platform: AppshotPlatform,
    pub global_shortcut: CapabilityState,
    pub window_capture: CapabilityState,
    pub application_text: CapabilityState,
    pub target: CaptureTarget,
}

impl AppshotCapabilities {
    pub fn setup_description(self) -> &'static str {
        match self.platform {
            AppshotPlatform::MacOs => {
                "Set up once, one permission at a time. Screen Recording captures the window; Accessibility optionally adds off-screen application text."
            }
            AppshotPlatform::Windows => {
                "Windows normally needs no advance setup. UI Automation adds application text when the target allows it."
            }
            AppshotPlatform::LinuxWayland => {
                "Your desktop owns capture and shortcut consent. Zeron checks each portal capability separately and explains any required fallback."
            }
            AppshotPlatform::LinuxX11 => {
                "X11 normally needs no capture permission. Zeron prefers an active-window screenshot portal when available and otherwise uses native X11 capture."
            }
            AppshotPlatform::Unsupported => {
                "This platform does not currently provide an Appshot capture backend."
            }
        }
    }

    pub fn shortcut_label(self) -> &'static str {
        match self.platform {
            AppshotPlatform::MacOs => "Control-Option-Space (⌃⌥Space)",
            AppshotPlatform::Windows => "Control-Alt-Space",
            AppshotPlatform::LinuxWayland | AppshotPlatform::LinuxX11 => "Control-Alt-Space",
            AppshotPlatform::Unsupported => "Unavailable",
        }
    }

    pub fn shortcut_description(self) -> &'static str {
        match self.platform {
            AppshotPlatform::MacOs | AppshotPlatform::Windows | AppshotPlatform::LinuxX11 => {
                "The shortcut works while another application has focus."
            }
            AppshotPlatform::LinuxWayland if self.global_shortcut == CapabilityState::Ready => {
                "Your desktop portal owns and delivers the global shortcut."
            }
            AppshotPlatform::LinuxWayland => {
                "Bind `zeron appshot` in your desktop's Keyboard Shortcuts settings."
            }
            AppshotPlatform::Unsupported => "This platform has no Appshot shortcut backend.",
        }
    }

    pub fn capture_description(self) -> &'static str {
        match (self.platform, self.target) {
            (AppshotPlatform::MacOs, _) => {
                "Screen Recording lets Zeron capture the frontmost window. macOS may request one restart."
            }
            (AppshotPlatform::Windows, _) => {
                "Windows captures the foreground compositor surface without opening a picker."
            }
            (AppshotPlatform::LinuxX11, _) => {
                "Zeron prefers portal active-window capture, then falls back to the X11 drawable; obscured or protected windows may be incomplete."
            }
            (AppshotPlatform::LinuxWayland, CaptureTarget::ActiveWindow) => {
                "Your screenshot portal supports the active-window target. A system consent surface may appear."
            }
            (AppshotPlatform::LinuxWayland, CaptureTarget::PortalWindowPicker) => {
                "Your portal requires choosing a window for each capture."
            }
            (AppshotPlatform::Unsupported, _) => {
                "Active-window capture is unavailable on this platform."
            }
        }
    }

    pub fn semantic_description(self) -> &'static str {
        match self.platform {
            AppshotPlatform::MacOs => {
                "Accessibility adds visible and off-screen application text. Screenshots work without it."
            }
            AppshotPlatform::Windows => {
                "UI Automation adds application text. Elevated and protected applications may return less context."
            }
            AppshotPlatform::LinuxWayland | AppshotPlatform::LinuxX11 => {
                "AT-SPI adds application text when the target exposes an accessibility tree."
            }
            AppshotPlatform::Unsupported => {
                "Semantic application text is unavailable on this platform."
            }
        }
    }
}

#[async_trait::async_trait]
pub trait AppshotBackend: Sync {
    fn capabilities(&self) -> AppshotCapabilities;
    fn start_global_shortcut(
        &self,
        activation_dir: &Path,
    ) -> futures::channel::mpsc::UnboundedReceiver<()>;
    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError>;
    fn request_capture_access(&self) {}
    fn request_semantic_access(&self) {}
    fn capture_settings_url(&self) -> Option<&'static str> {
        None
    }
    fn semantic_settings_url(&self) -> Option<&'static str> {
        None
    }
}

#[cfg(target_os = "macos")]
static BACKEND: macos::MacOsBackend = macos::MacOsBackend;
#[cfg(target_os = "windows")]
static BACKEND: windows::WindowsBackend = windows::WindowsBackend;
#[cfg(target_os = "linux")]
static BACKEND: linux::LinuxBackend = linux::LinuxBackend;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct UnsupportedBackend;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
#[async_trait::async_trait]
impl AppshotBackend for UnsupportedBackend {
    fn capabilities(&self) -> AppshotCapabilities {
        AppshotCapabilities {
            platform: AppshotPlatform::Unsupported,
            global_shortcut: CapabilityState::Unavailable,
            window_capture: CapabilityState::Unavailable,
            application_text: CapabilityState::Unavailable,
            target: CaptureTarget::ActiveWindow,
        }
    }

    fn start_global_shortcut(
        &self,
        _activation_dir: &Path,
    ) -> futures::channel::mpsc::UnboundedReceiver<()> {
        futures::channel::mpsc::unbounded().1
    }

    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError> {
        Err(CaptureError::CaptureFailed(
            "Appshots are not available on this platform.".into(),
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
static BACKEND: UnsupportedBackend = UnsupportedBackend;

fn backend() -> &'static dyn AppshotBackend {
    &BACKEND
}

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

pub fn validate_capture_dimensions(width: u32, height: u32) -> Result<usize, CaptureError> {
    if width == 0 || height == 0 || width > MAX_CAPTURE_DIMENSION || height > MAX_CAPTURE_DIMENSION
    {
        return Err(CaptureError::CaptureFailed(format!(
            "The captured window dimensions ({width}×{height}) are not supported."
        )));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| CaptureError::CaptureFailed("The captured window is too large.".into()))?;
    let rgba_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| CaptureError::CaptureFailed("The captured window is too large.".into()))?;
    if pixels > MAX_CAPTURE_PIXELS || rgba_bytes > MAX_CAPTURE_RGBA_BYTES {
        return Err(CaptureError::CaptureFailed(format!(
            "The captured window ({width}×{height}) exceeds Zeron's capture budget."
        )));
    }
    usize::try_from(rgba_bytes)
        .map_err(|_| CaptureError::CaptureFailed("The captured window is too large.".into()))
}

/// Turn OS-provided application names into one safe filename component.
pub fn safe_app_name(value: &str) -> String {
    let mut safe = String::with_capacity(value.len().min(100));
    for ch in value.chars() {
        if safe.chars().count() >= 100 {
            break;
        }
        if ch.is_control() || matches!(ch, '/' | '\\' | ':') {
            safe.push('-');
        } else {
            safe.push(ch);
        }
    }
    let safe = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = safe.trim_matches(['.', '-', ' ']);
    if safe.is_empty() {
        "Application".into()
    } else {
        safe.into()
    }
}

pub fn stage_appshot_png(
    app_name: &str,
    bytes: Vec<u8>,
) -> Result<(StagedAttachment, (u32, u32)), CaptureError> {
    if bytes.len() as u64 > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(CaptureError::CaptureFailed(
            "The captured window is larger than Zeron's 24 MB image limit.".into(),
        ));
    }
    let dimensions = png_dimensions(&bytes).ok_or_else(|| {
        CaptureError::CaptureFailed("The captured window is not a valid PNG image.".into())
    })?;
    validate_capture_dimensions(dimensions.0, dimensions.1)?;
    Ok((
        crate::attachments::stage_png_bytes(
            format!("{} Appshot.png", safe_app_name(app_name)),
            bytes,
        ),
        dimensions,
    ))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
struct AttachmentBudgetWriter {
    bytes: Vec<u8>,
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl Write for AttachmentBudgetWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(input.len())
            > crate::attachments::MAX_ATTACHMENT_BYTES as usize
        {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "encoded Appshot exceeds attachment budget",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn encode_rgba_png(
    width: u32,
    height: u32,
    rgba: &[u8],
    platform: &str,
) -> Result<Vec<u8>, CaptureError> {
    let expected = validate_capture_dimensions(width, height)?;
    if rgba.len() != expected {
        return Err(CaptureError::CaptureFailed(format!(
            "{platform} returned an invalid pixel buffer."
        )));
    }
    let mut output = AttachmentBudgetWriter { bytes: Vec::new() };
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(rgba))
            .map_err(|error| {
                CaptureError::CaptureFailed(format!("{platform} Appshot encoding failed: {error}"))
            })?;
    }
    Ok(output.bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    PermissionRequired,
    NoEligibleWindow,
    ShortcutUnavailable,
    CaptureFailed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionRequired => f.write_str(
                "Window capture permission is required. Open Zeron Settings → Shortcuts for the platform-specific recovery step.",
            ),
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

pub fn capabilities() -> AppshotCapabilities {
    backend().capabilities()
}

pub fn request_capture_access() {
    backend().request_capture_access();
}

pub fn request_semantic_access() {
    backend().request_semantic_access();
}

pub fn capture_settings_url() -> Option<&'static str> {
    backend().capture_settings_url()
}

pub fn semantic_settings_url() -> Option<&'static str> {
    backend().semantic_settings_url()
}

/// Register the platform global shortcut and, on Linux, the local activation
/// socket used by `zeron appshot` when the desktop owns shortcut setup.
pub fn start_global_shortcut(
    activation_dir: PathBuf,
) -> futures::channel::mpsc::UnboundedReceiver<()> {
    backend().start_global_shortcut(&activation_dir)
}

pub async fn capture_active_window() -> Result<CapturedAppshot, CaptureError> {
    backend().capture_active_window().await
}

/// Ask the running headed Zeron process to capture an Appshot. Linux desktop
/// environments that do not implement the Global Shortcuts portal can bind
/// `zeron appshot` in their native Keyboard Shortcuts settings.
#[cfg(target_os = "linux")]
pub fn request_running_appshot(data_dir: &Path) -> Result<(), CaptureError> {
    linux::request_running_appshot(data_dir)
}

#[cfg(not(target_os = "linux"))]
pub fn request_running_appshot(_data_dir: &Path) -> Result<(), CaptureError> {
    Err(CaptureError::ShortcutUnavailable)
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
    fn capture_dimensions_and_names_are_bounded_before_staging() {
        assert_eq!(validate_capture_dimensions(4096, 4096), Ok(4096 * 4096 * 4));
        assert!(validate_capture_dimensions(8193, 1).is_err());
        assert!(validate_capture_dimensions(8192, 8192).is_err());
        assert_eq!(safe_app_name("Bad\nApp/../../name"), "Bad-App-..-..-name");
        assert_eq!(safe_app_name("\0\n"), "Application");
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

    #[test]
    fn wayland_capabilities_explain_picker_and_system_shortcut_fallbacks() {
        let capabilities = AppshotCapabilities {
            platform: AppshotPlatform::LinuxWayland,
            global_shortcut: CapabilityState::SetupRequired,
            window_capture: CapabilityState::UserSelection,
            application_text: CapabilityState::Ready,
            target: CaptureTarget::PortalWindowPicker,
        };
        assert!(
            capabilities
                .shortcut_description()
                .contains("zeron appshot")
        );
        assert!(capabilities.capture_description().contains("each capture"));
        assert!(capabilities.window_capture.is_ready());
    }
}
