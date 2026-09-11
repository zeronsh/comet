//! Appshots: user-triggered captures of the frontmost application window.
//!
//! Capture stays on the headed/viewer device. The screenshot reuses the
//! ordinary attachment transport; accessibility-derived application context
//! is serialized into the prompt as explicitly untrusted observed data.

use std::collections::HashMap;
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

mod shortcut;
pub(crate) use shortcut::capture_allowed;
pub use shortcut::{set_recording, set_shortcut, validate_shortcut};

pub const fn is_desktop() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

static CAPTURE_SOUND_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_capture_sound_enabled(enabled: bool) {
    CAPTURE_SOUND_ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        crate::sound::prepare_appshot();
    }
}

/// Acknowledge saved pixels immediately; optional semantic enrichment may still be running.
fn capture_ready() {
    if CAPTURE_SOUND_ENABLED.load(Ordering::Relaxed) {
        crate::sound::play_appshot();
    }
}

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

    pub fn shortcut_description(self) -> &'static str {
        match self.platform {
            AppshotPlatform::MacOs | AppshotPlatform::LinuxX11
                if self.global_shortcut != CapabilityState::Ready =>
            {
                "This shortcut is unavailable. Choose a different key combination."
            }
            AppshotPlatform::MacOs | AppshotPlatform::LinuxX11 => {
                "The shortcut works while another application has focus."
            }
            AppshotPlatform::LinuxWayland if self.global_shortcut == CapabilityState::Ready => {
                "Your desktop portal controls the binding. Confirm changes in its shortcut settings."
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
            (AppshotPlatform::LinuxX11, _) => {
                "Zeron uses native X11 capture when active-window portal capture is unavailable. Obscured or protected windows may be incomplete."
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
            AppshotPlatform::LinuxWayland => {
                "This portal does not identify the captured window, so Appshots include the screenshot only."
            }
            AppshotPlatform::LinuxX11 => {
                "Native X11 captures can include AT-SPI text when the process and window can be matched uniquely. Portal captures include the screenshot only."
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
#[cfg(target_os = "linux")]
static BACKEND: linux::LinuxBackend = linux::LinuxBackend;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
struct UnsupportedBackend;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// Native capture surfaces can contain transparent padding (for example Chrome
/// can return a wider backing surface than its visible window). Crop only rows
/// and columns that contain no nonzero alpha. Do this before deriving thumbnail
/// dimensions, so preview, upload and restored queue drafts share the same image.
/// Opaque margins, rounded corners and even alpha=1 pixels remain untouched.
fn trim_appshot_padding(bytes: &[u8]) -> Result<Option<Vec<u8>>, CaptureError> {
    let invalid = |error: png::DecodingError| {
        CaptureError::CaptureFailed(format!("Could not decode the captured window: {error}"))
    };
    if bytes.len() as u64 > crate::attachments::MAX_ATTACHMENT_BYTES {
        return Err(CaptureError::CaptureFailed(
            "The captured window is larger than Zeron's 24 MB image limit.".into(),
        ));
    }
    let (width, height) = png_dimensions(bytes).ok_or_else(|| {
        CaptureError::CaptureFailed("The captured window is not a valid PNG image.".into())
    })?;
    validate_capture_dimensions(width, height)?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_CAPTURE_RGBA_BYTES as usize,
    });
    // Expand palette/transparency entries, but retain 16-bit channel precision.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(invalid)?;
    let (color, _) = reader.output_color_type();
    if reader.info().is_animated()
        || !matches!(color, png::ColorType::Rgba | png::ColorType::GrayscaleAlpha)
    {
        return Ok(None);
    }
    let buffer_size = reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_CAPTURE_RGBA_BYTES as usize)
        .ok_or_else(|| CaptureError::CaptureFailed("The captured window is too large.".into()))?;
    let mut pixels = vec![0; buffer_size];
    let frame = reader.next_frame(&mut pixels).map_err(invalid)?;
    let sample_bytes = frame.bit_depth as usize / 8;
    let pixel_bytes = frame.color_type.samples() * sample_bytes;
    let alpha_offset = pixel_bytes - sample_bytes;
    let visible = |x: u32, y: u32| {
        let start = (y as usize * width as usize + x as usize) * pixel_bytes + alpha_offset;
        pixels[start..start + sample_bytes]
            .iter()
            .any(|alpha| *alpha != 0)
    };
    // Search inward from the edges. Ordinary opaque captures only inspect a
    // handful of pixels instead of scanning the whole Retina-sized surface.
    let Some(top) = (0..height).find(|y| (0..width).any(|x| visible(x, *y))) else {
        return Ok(None);
    };
    let bottom = (top..height)
        .rfind(|y| (0..width).any(|x| visible(x, *y)))
        .unwrap()
        + 1;
    let left = (0..width)
        .find(|x| (top..bottom).any(|y| visible(*x, y)))
        .unwrap();
    let right = (left..width)
        .rfind(|x| (top..bottom).any(|y| visible(*x, y)))
        .unwrap()
        + 1;
    if left == 0 && top == 0 && right == width && bottom == height {
        return Ok(None);
    }
    let cropped_width = right - left;
    let cropped_height = bottom - top;
    let row_bytes = cropped_width as usize * pixel_bytes;
    // Compact in place rather than allocating a second full-sized pixel buffer.
    for row in 0..cropped_height as usize {
        let start = ((row + top as usize) * width as usize + left as usize) * pixel_bytes;
        pixels.copy_within(start..start + row_bytes, row * row_bytes);
    }
    pixels.truncate(row_bytes * cropped_height as usize);
    let source = reader.info();
    let mut info = png::Info::with_size(cropped_width, cropped_height);
    info.color_type = frame.color_type;
    info.bit_depth = frame.bit_depth;
    info.pixel_dims = source.pixel_dims;
    info.source_gamma = source.source_gamma;
    info.source_chromaticities = source.source_chromaticities;
    info.srgb = source.srgb;
    info.icc_profile = source.icc_profile.clone();
    info.coding_independent_code_points = source.coding_independent_code_points;
    info.mastering_display_color_volume = source.mastering_display_color_volume;
    info.content_light_level = source.content_light_level;
    let mut output = AttachmentBudgetWriter { bytes: Vec::new() };
    png::Encoder::with_info(&mut output, info)
        .and_then(|encoder| encoder.write_header())
        .and_then(|mut writer| writer.write_image_data(&pixels))
        .map_err(|error| {
            CaptureError::CaptureFailed(format!("Could not encode the captured window: {error}"))
        })?;
    Ok(Some(output.bytes))
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
    let bytes = trim_appshot_padding(&bytes)?.unwrap_or(bytes);
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

struct AttachmentBudgetWriter {
    bytes: Vec<u8>,
}

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

#[cfg(target_os = "linux")]
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
    Cancelled,
    SelfCapture,
    NoEligibleWindow,
    ShortcutUnavailable,
    CaptureFailed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionRequired => f.write_str(
                "Window capture permission is required. Open Zeron Settings → Appshots for the platform-specific recovery step.",
            ),
            Self::Cancelled => f.write_str("Appshot capture cancelled."),
            Self::SelfCapture => f.write_str("Switch to another app to capture an Appshot."),
            Self::NoEligibleWindow => f.write_str("No application window is available to capture."),
            Self::ShortcutUnavailable => f.write_str(
                "The Appshot shortcut could not be registered because another app may be using it.",
            ),
            Self::CaptureFailed(message) => f.write_str(message),
        }
    }
}

pub fn set_enabled(enabled: bool) {
    if ENABLED.swap(enabled && is_desktop(), Ordering::Relaxed) != (enabled && is_desktop()) {
        shortcut::enabled_changed();
    }
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
    let result = backend().capture_active_window().await;
    #[cfg(target_os = "linux")]
    if result.is_ok() {
        capture_ready();
    }
    result
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

/// Complete the user-requested capture handoff after staging, on the UI thread.
pub fn foreground_after_capture() {
    #[cfg(target_os = "macos")]
    macos::foreground_after_capture();
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
            '\r' => escaped.push_str("&#13;"),
            '\n' => escaped.push_str("&#10;"),
            '\t' => escaped.push_str("&#9;"),
            // XML 1.0 cannot represent these characters, even as numeric
            // references. Native application text may contain them; retain a
            // visible replacement instead of making the whole capture unreadable.
            '\u{0}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => escaped.push('\u{fffd}'),
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

/// Restore the serialized Appshots in a queued message onto its loaded images.
/// Reject malformed or unmatched metadata so editing cannot silently drop it.
/// Icons and capture timestamps are presentation-only and were never transported.
pub(crate) fn restore_queued_appshots(
    text: &str,
    paths: &[String],
    attachments: &[StagedAttachment],
) -> Result<(Vec<StagedAttachment>, Vec<CapturedAppshot>), String> {
    let marker = format!("\n\n{CONTEXT_MARKER}");
    let Some((_, context)) = text.split_once(&marker) else {
        return Ok((attachments.to_vec(), Vec::new()));
    };
    let invalid = || {
        "Couldn't restore this Appshot's context. Cancel and retry from the original device."
            .to_string()
    };
    if paths.len() != attachments.len() || context.len() > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    // Older hosts retained the ordinary attachment trailer inside queue text.
    let context = context
        .split("\n\nAttached images (local files")
        .next()
        .unwrap_or(context);
    let xml = format!("<appshots>{context}</appshots>");
    let doc = roxmltree::Document::parse_with_options(
        &xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 4096,
            ..Default::default()
        },
    )
    .map_err(|_| invalid())?;
    let mut used = std::collections::HashSet::new();
    let mut shots = Vec::new();
    for node in doc.root_element().children() {
        if node.is_text() && node.text().unwrap_or_default().trim().is_empty() {
            continue;
        }
        if !node.has_tag_name("appshot") || node.children().any(|child| !child.is_text()) {
            return Err(invalid());
        }
        let image = node.attribute("image").ok_or_else(invalid)?;
        let index = paths
            .iter()
            .position(|path| path == image)
            .ok_or_else(invalid)?;
        if !used.insert(index) {
            return Err(invalid());
        }
        let mut screenshot = attachments[index].clone();
        // Older captures may already have backing-surface padding stored in
        // their PNG. Normalize their bytes too, without changing attachment IDs.
        if png_dimensions(screenshot.bytes()).is_some() {
            if let Some(bytes) = trim_appshot_padding(screenshot.bytes()).map_err(|_| invalid())? {
                screenshot.image = Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes));
            }
        }
        let content = node.text().unwrap_or_default();
        // Remove only the serializer's surrounding newlines, preserving content.
        let content = content.strip_prefix('\n').unwrap_or(content);
        let content = content.strip_suffix('\n').unwrap_or(content);
        shots.push(CapturedAppshot {
            id: uuid::Uuid::new_v4().to_string(),
            app_name: node.attribute("app").ok_or_else(invalid)?.to_string(),
            bundle_identifier: node.attribute("bundle-identifier").map(str::to_string),
            window_title: node.attribute("window-title").map(str::to_string),
            accessibility: AccessibilitySnapshot {
                format_version: node
                    .attribute("accessibility-format")
                    .ok_or_else(invalid)?
                    .parse()
                    .map_err(|_| invalid())?,
                content: content.to_string(),
                truncated: node
                    .attribute("truncated")
                    .ok_or_else(invalid)?
                    .parse()
                    .map_err(|_| invalid())?,
            },
            screenshot_dimensions: png_dimensions(screenshot.bytes()),
            screenshot,
            app_icon: None,
            captured_at: Utc::now(),
        });
    }
    if shots.is_empty()
        || shots
            .iter()
            .map(|shot| shot.screenshot.bytes().len() as u64)
            .sum::<u64>()
            > MAX_STAGED_APPSHOT_BYTES
    {
        return Err(invalid());
    }
    let ordinary = attachments
        .iter()
        .enumerate()
        .filter(|(index, _)| !used.contains(index))
        .map(|(_, attachment)| attachment.clone())
        .collect();
    Ok((ordinary, shots))
}

/// Safe display metadata carried by the existing prompt format. The observed
/// accessibility payload is never returned to the transcript or queue UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppshotPresentation {
    pub app_name: String,
    pub window_title: Option<String>,
    pub bundle_identifier: Option<String>,
}

impl AppshotPresentation {
    pub fn title(&self) -> &str {
        self.window_title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.app_name)
    }
}

/// Resolve an installed app's icon locally; remote/mobile viewers retain the
/// app-name badge when that application is not installed. Icons never enter
/// the model prompt or add another attachment to the transport.
pub fn presentation_icon(presentation: &AppshotPresentation) -> Option<Arc<gpui::Image>> {
    #[cfg(target_os = "macos")]
    {
        static ICONS: std::sync::OnceLock<
            std::sync::Mutex<HashMap<String, Option<Arc<gpui::Image>>>>,
        > = std::sync::OnceLock::new();
        let bundle = presentation.bundle_identifier.as_deref()?;
        let mut icons = ICONS.get_or_init(Default::default).lock().ok()?;
        if let Some(icon) = icons.get(bundle) {
            return icon.clone();
        }
        if icons.len() >= 64 {
            return None;
        }
        let icon = macos::icon_for_bundle(bundle).and_then(|bytes| {
            crate::attachments::queue_thumbnail_image(&gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                bytes,
            ))
        });
        icons.insert(bundle.to_string(), icon.clone());
        icon
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = presentation;
        None
    }
}

pub fn presentations(text: &str) -> HashMap<String, AppshotPresentation> {
    let marker = format!("\n\n{CONTEXT_MARKER}");
    let Some((_, context)) = text.split_once(&marker) else {
        return HashMap::new();
    };
    if context.len() > 4 * 1024 * 1024 {
        return HashMap::new();
    }
    let context = context
        .split("\n\nAttached images (local files")
        .next()
        .unwrap_or("");
    let xml = format!("<appshots>{context}</appshots>");
    let Ok(doc) = roxmltree::Document::parse_with_options(
        &xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 4096,
            ..Default::default()
        },
    ) else {
        return HashMap::new();
    };
    let mut result = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for node in doc
        .root_element()
        .children()
        .filter(|node| node.is_element())
    {
        if !node.has_tag_name("appshot") || node.children().any(|child| !child.is_text()) {
            continue;
        }
        let (Some(path), Some(app)) = (node.attribute("image"), node.attribute("app")) else {
            continue;
        };
        if !seen.insert(path) {
            result.remove(path);
            continue;
        }
        if path.is_empty() || app.trim().is_empty() {
            continue;
        }
        result.insert(
            path.to_string(),
            AppshotPresentation {
                app_name: app.chars().take(200).collect(),
                window_title: node
                    .attribute("window-title")
                    .map(|value| value.chars().take(512).collect()),
                bundle_identifier: node.attribute("bundle-identifier").map(str::to_string),
            },
        );
    }
    result
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
pub(crate) mod tests {
    use super::*;
    use gpui::{Image, ImageFormat};
    use std::sync::Arc;

    pub(crate) fn shot() -> CapturedAppshot {
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

    fn fixture_png(width: u32, height: u32, pixels: &[u8], depth: png::BitDepth) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(depth);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(pixels)
            .unwrap();
        bytes
    }

    fn decoded_pixels(
        bytes: &[u8],
    ) -> (png::OutputInfo, Vec<u8>, Option<png::SrgbRenderingIntent>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let output = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(output.buffer_size());
        (output, pixels, reader.info().srgb)
    }

    #[test]
    fn capture_staging_removes_chrome_backing_surface_padding_before_layout() {
        // Same failure as the supplied Chrome capture: a wider PNG canvas than
        // visible content, with transparent columns exclusively on the right.
        let mut pixels = vec![0; 302 * 165 * 4];
        for row in pixels.chunks_exact_mut(302 * 4) {
            for pixel in row[..264 * 4].chunks_exact_mut(4) {
                pixel.copy_from_slice(&[22, 33, 44, 255]);
            }
        }
        let png = fixture_png(302, 165, &pixels, png::BitDepth::Eight);
        let (staged, dimensions) = stage_appshot_png("Google Chrome", png).unwrap();
        assert_eq!(dimensions, (264, 165));
        assert_eq!(png_dimensions(staged.bytes()), Some(dimensions));
        let (_, decoded, profile) = decoded_pixels(staged.bytes());
        assert_eq!(decoded, [22, 33, 44, 255].repeat(264 * 165));
        assert_eq!(profile, Some(png::SrgbRenderingIntent::Perceptual));
        assert!(trim_appshot_padding(staged.bytes()).unwrap().is_none());
        let (width, height) = crate::composer::appshot_contained_size(Some(dimensions), 320.0);
        assert!((width / height - 264.0 / 165.0).abs() < 0.0001);
    }

    #[test]
    fn padding_trim_preserves_rounded_corners_faint_pixels_and_interior_transparency() {
        let mut pixels = vec![0; 8 * 7 * 4];
        for y in 1..6 {
            for x in 2..7 {
                pixels[(y * 8 + x) * 4..(y * 8 + x + 1) * 4].copy_from_slice(&[12, 34, 56, 255]);
            }
        }
        // A rounded corner, a transparent interior hole, and the faintest
        // possible nonzero-alpha edge must survive unchanged.
        pixels[(1 * 8 + 2) * 4 + 3] = 0;
        pixels[(3 * 8 + 4) * 4 + 3] = 0;
        pixels[(3 * 8 + 2) * 4 + 3] = 1;
        let png = fixture_png(8, 7, &pixels, png::BitDepth::Eight);
        let trimmed = trim_appshot_padding(&png).unwrap().unwrap();
        let (info, actual, _) = decoded_pixels(&trimmed);
        assert_eq!((info.width, info.height), (5, 5));
        let expected: Vec<u8> = (1..6)
            .flat_map(|y| pixels[(y * 8 + 2) * 4..(y * 8 + 7) * 4].to_vec())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn padding_trim_keeps_opaque_margins_and_empty_images_byte_identical() {
        for pixel in [[0, 0, 0, 255], [255, 255, 255, 255], [23, 45, 67, 0]] {
            let png = fixture_png(8, 6, &pixel.repeat(48), png::BitDepth::Eight);
            assert!(trim_appshot_padding(&png).unwrap().is_none());
            assert_eq!(
                stage_appshot_png("Discord", png.clone()).unwrap().0.bytes(),
                png
            );
        }
    }

    #[test]
    fn padding_trim_preserves_16_bit_color_and_alpha_precision() {
        let mut pixels = vec![0; 3 * 2 * 8];
        let sample = [0x12, 0x34, 0xab, 0xcd, 0x56, 0x78, 0x00, 0x01];
        pixels[8..16].copy_from_slice(&sample);
        let png = fixture_png(3, 2, &pixels, png::BitDepth::Sixteen);
        let trimmed = trim_appshot_padding(&png).unwrap().unwrap();
        let (info, actual, _) = decoded_pixels(&trimmed);
        assert_eq!(
            (info.width, info.height, info.bit_depth),
            (1, 1, png::BitDepth::Sixteen)
        );
        assert_eq!(actual, sample);
    }

    #[test]
    fn restored_queue_appshots_trim_legacy_padding_without_changing_attachment_identity() {
        let pixels = [[0, 0, 0, 0], [44, 55, 66, 255], [0, 0, 0, 0]].concat();
        let mut original = shot();
        original.screenshot = crate::attachments::stage_png_bytes(
            "Chrome.png".into(),
            fixture_png(3, 1, &pixels, png::BitDepth::Eight),
        );
        let path = "/host/legacy.png".to_string();
        let text = with_appshots(
            "edit",
            &[original.clone()],
            &HashMap::from([(original.screenshot.id.clone(), path.clone())]),
        );
        let (_, restored) =
            restore_queued_appshots(&text, &[path], &[original.screenshot.clone()]).unwrap();
        assert_eq!(restored[0].screenshot.id, original.screenshot.id);
        assert_eq!(restored[0].screenshot_dimensions, Some((1, 1)));
        assert_eq!(restored[0].accessibility, original.accessibility);
        assert_eq!(
            decoded_pixels(restored[0].screenshot.bytes()).1,
            [44, 55, 66, 255]
        );
        // The original upload remains immutable.
        assert_eq!(png_dimensions(original.screenshot.bytes()), Some((3, 1)));
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
    fn appshot_invalid_xml_characters_round_trip_through_queue_and_presentation() {
        let mut original = shot();
        original.app_name = "App\0 & Notes".into();
        original.window_title = Some("Title\u{1b}\u{fffe}\u{ffff}".into());
        original.accessibility.content = format!(
            "{}valid\t\r\n<&>é🦀\u{7f}\u{85}\u{10000}",
            (0..32).filter_map(char::from_u32).collect::<String>()
        );
        let path = "/host/image.png".to_string();
        let encoded = with_appshots(
            "inspect",
            &[original.clone()],
            &HashMap::from([(original.screenshot.id.clone(), path.clone())]),
        );
        let sources = presentations(&encoded);
        assert_eq!(sources[&path].app_name, "App� & Notes");
        assert_eq!(sources[&path].window_title.as_deref(), Some("Title���"));
        let (_, restored) =
            restore_queued_appshots(&encoded, &[path], &[original.screenshot.clone()]).unwrap();
        assert_eq!(restored[0].app_name, "App� & Notes");
        assert_eq!(restored[0].window_title.as_deref(), Some("Title���"));
        assert_eq!(
            restored[0].accessibility.content,
            format!(
                "{}\t\n{}\r{}valid\t\r\n<&>é🦀\u{7f}\u{85}\u{10000}",
                "�".repeat(9),
                "�".repeat(2),
                "�".repeat(18)
            )
        );
    }

    #[test]
    fn queued_edit_round_trip_keeps_context_and_rebinds_uploaded_images() {
        let mut original = shot();
        original.window_title = Some("Line 1\nLine 2\t&\"".into());
        original.accessibility.content = "\n<&secret>\r\n  text\n".into();
        let mut ordinary = original.screenshot.clone();
        ordinary.id = "ordinary".into();
        let paths: Vec<String> = vec!["/host/ordinary.png".into(), "/host/a&b.png".into()];
        let encoded = with_appshots(
            "inspect",
            &[original.clone()],
            &HashMap::from([(original.screenshot.id.clone(), paths[1].clone())]),
        );
        for text in [
            encoded.clone(),
            crate::attachments::with_attachments(&encoded, &paths),
        ] {
            let (ordinary_restored, restored) = restore_queued_appshots(
                &text,
                &paths,
                &[ordinary.clone(), original.screenshot.clone()],
            )
            .unwrap();
            assert_eq!(ordinary_restored.len(), 1);
            assert_eq!(ordinary_restored[0].id, "ordinary");
            assert_eq!(restored.len(), 1);
            assert_eq!(restored[0].accessibility, original.accessibility);
            assert_eq!(restored[0].window_title, original.window_title);
            assert_eq!(restored[0].app_name, original.app_name);
            let rebound = with_appshots(
                "edited",
                &restored,
                &HashMap::from([(restored[0].screenshot.id.clone(), "/new/renamed.png".into())]),
            );
            assert!(rebound.contains("image=\"/new/renamed.png\""));
            assert!(!rebound.contains("/host/"));
            assert_eq!(strip_context_for_display(&rebound), "edited");
            assert_eq!(with_appshots("edited", &[], &HashMap::new()), "edited");
        }
    }

    #[test]
    fn queued_edit_rejects_invalid_or_unmatched_context_without_losing_images() {
        let original = shot();
        let paths: Vec<String> = vec!["/host/image.png".into()];
        let valid = with_appshots(
            "",
            &[original.clone()],
            &HashMap::from([(original.screenshot.id.clone(), paths[0].clone())]),
        );
        for invalid in [
            valid.replace("/host/image.png", "/missing.png"),
            valid.replace("</appshot>", "</broken>"),
            valid.replace("<appshot ", "<other "),
            format!("{valid}\n{}", valid.split_once(CONTEXT_MARKER).unwrap().1),
        ] {
            assert!(
                restore_queued_appshots(&invalid, &paths, &[original.screenshot.clone()]).is_err()
            );
        }
        let (ordinary, shots) =
            restore_queued_appshots("plain", &paths, &[original.screenshot]).unwrap();
        assert_eq!(ordinary.len(), 1);
        assert!(shots.is_empty());
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
