use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use gpui::{Image, ImageFormat as GpuiImageFormat};
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HGDIOBJ, SelectObject,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{
    DI_NORMAL, DestroyIcon, DrawIconEx, GetForegroundWindow, GetMessageW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, MSG, WM_HOTKEY,
};
use windows::core::{PCWSTR, PWSTR};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window as CaptureWindow;

use super::{
    AccessibilitySnapshot, AppshotBackend, AppshotCapabilities, AppshotPlatform, CapabilityState,
    CaptureError, CaptureTarget, CapturedAppshot, validate_capture_dimensions,
};

const HOTKEY_ID: i32 = 0x5A_41_50;
const VK_SPACE: u32 = 0x20;
const MAX_UIA_DEPTH: usize = 24;
const MAX_UIA_NODES: usize = 1_500;
const MAX_UIA_BYTES: usize = 96 * 1024;
const MAX_UIA_TEXT: i32 = 4_096;
const UIA_DEADLINE: Duration = Duration::from_millis(900);
static SHORTCUT_READY: AtomicBool = AtomicBool::new(true);

pub struct WindowsBackend;

#[async_trait::async_trait]
impl AppshotBackend for WindowsBackend {
    fn capabilities(&self) -> AppshotCapabilities {
        let capture_supported = GraphicsCaptureSession::IsSupported().unwrap_or(false);
        AppshotCapabilities {
            platform: AppshotPlatform::Windows,
            global_shortcut: if SHORTCUT_READY.load(Ordering::Relaxed) {
                CapabilityState::Ready
            } else {
                CapabilityState::SetupRequired
            },
            window_capture: if capture_supported {
                CapabilityState::Ready
            } else {
                CapabilityState::Unavailable
            },
            application_text: CapabilityState::Ready,
            target: CaptureTarget::ActiveWindow,
        }
    }

    fn start_global_shortcut(&self, _activation_dir: &Path) -> mpsc::UnboundedReceiver<()> {
        start_global_shortcut()
    }

    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError> {
        capture_foreground_window()
    }
}

fn start_global_shortcut() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded();
    thread::Builder::new()
        .name("appshot-windows-hotkey".into())
        .spawn(move || unsafe {
            if RegisterHotKey(
                None,
                HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_SPACE,
            )
            .is_err()
            {
                SHORTCUT_READY.store(false, Ordering::Relaxed);
                tracing::warn!("Windows Appshot shortcut is already in use");
                return;
            }
            SHORTCUT_READY.store(true, Ordering::Relaxed);
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                if message.message == WM_HOTKEY
                    && message.wParam.0 == HOTKEY_ID as usize
                    && tx.unbounded_send(()).is_err()
                {
                    break;
                }
            }
            let _ = UnregisterHotKey(None, HOTKEY_ID);
        })
        .ok();
    rx
}

fn capture_foreground_window() -> Result<CapturedAppshot, CaptureError> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(CaptureError::NoEligibleWindow);
    }
    let metadata = window_metadata(hwnd);
    let screenshot_png = capture_window_png(hwnd)?;
    let accessibility = capture_uia(hwnd).unwrap_or_else(|error| {
        tracing::debug!(?error, "UI Automation context unavailable for Appshot");
        AccessibilitySnapshot::unavailable()
    });
    let app_name = metadata
        .app_name
        .unwrap_or_else(|| "Windows application".into());
    let (screenshot, dimensions) = super::stage_appshot_png(&app_name, screenshot_png)?;
    Ok(CapturedAppshot {
        id: uuid::Uuid::new_v4().to_string(),
        app_name,
        bundle_identifier: metadata.application_user_model_id.or_else(|| {
            metadata
                .executable
                .as_ref()
                .map(|path| format!("windows:{}", path.display()))
        }),
        window_title: metadata.title,
        accessibility,
        screenshot,
        screenshot_dimensions: Some(dimensions),
        app_icon: metadata
            .icon_png
            .map(|bytes| Arc::new(Image::from_bytes(GpuiImageFormat::Png, bytes))),
        captured_at: chrono::Utc::now(),
    })
}

struct WindowMetadata {
    title: Option<String>,
    app_name: Option<String>,
    executable: Option<PathBuf>,
    application_user_model_id: Option<String>,
    icon_png: Option<Vec<u8>>,
}

fn window_metadata(hwnd: HWND) -> WindowMetadata {
    let title = unsafe {
        let length = GetWindowTextLengthW(hwnd);
        let mut buffer = vec![0_u16; (length + 1).max(1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buffer);
        (copied > 0).then(|| String::from_utf16_lossy(&buffer[..copied as usize]))
    };
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let (executable, application_user_model_id) =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .ok()
            .map(|handle| {
                let mut buffer = vec![0_u16; 32_768];
                let mut length = buffer.len() as u32;
                let executable = unsafe {
                    QueryFullProcessImageNameW(
                        handle,
                        PROCESS_NAME_FORMAT(0),
                        PWSTR(buffer.as_mut_ptr()),
                        &mut length,
                    )
                }
                .ok()
                .map(|_| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])));
                let application_user_model_id = application_user_model_id(handle);
                let _ = unsafe { CloseHandle(handle) };
                (executable, application_user_model_id)
            })
            .unwrap_or((None, None));
    let app_name = executable
        .as_ref()
        .and_then(|path| path.file_stem())
        .and_then(|value| value.to_str())
        .map(str::to_string);
    let icon_png = executable.as_deref().and_then(executable_icon_png);
    WindowMetadata {
        title,
        app_name,
        executable,
        application_user_model_id,
        icon_png,
    }
}

fn application_user_model_id(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    let mut length = 0_u32;
    let _ = unsafe { GetApplicationUserModelId(handle, &mut length, None) };
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; length as usize];
    let status =
        unsafe { GetApplicationUserModelId(handle, &mut length, Some(PWSTR(buffer.as_mut_ptr()))) };
    (status.0 == 0).then(|| {
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    })
}

struct OneShotCapture {
    sender: std_mpsc::SyncSender<Result<(u32, u32, Vec<u8>), String>>,
}

impl GraphicsCaptureApiHandler for OneShotCapture {
    type Flags = std_mpsc::SyncSender<Result<(u32, u32, Vec<u8>), String>>;
    type Error = String;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            sender: context.flags,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        if let Err(error) = validate_capture_dimensions(width, height) {
            let _ = self.sender.send(Err(error.to_string()));
            capture_control.stop();
            return Ok(());
        }
        let pixels = frame
            .buffer()
            .and_then(|mut buffer| buffer.as_nopadding_buffer().map(|pixels| pixels.to_vec()))
            .map_err(|error| error.to_string());
        let _ = self
            .sender
            .send(pixels.map(|pixels| (width, height, pixels)));
        capture_control.stop();
        Ok(())
    }
}

fn capture_window_png(hwnd: HWND) -> Result<Vec<u8>, CaptureError> {
    let (sender, receiver) = std_mpsc::sync_channel(1);
    let settings = Settings::new(
        CaptureWindow::from_raw_hwnd(hwnd.0),
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        sender,
    );
    OneShotCapture::start(settings).map_err(failed)?;
    let (width, height, rgba) = receiver
        .recv_timeout(Duration::from_secs(4))
        .map_err(|error| {
            CaptureError::CaptureFailed(format!("Windows capture timed out: {error}"))
        })?
        .map_err(CaptureError::CaptureFailed)?;
    encode_rgba(width, height, &rgba)
}

fn capture_uia(hwnd: HWND) -> windows::core::Result<AccessibilitySnapshot> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = ComApartment;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let root = unsafe { automation.ElementFromHandle(hwnd)? };
    let walker = unsafe { automation.ControlViewWalker()? };
    let started = Instant::now();
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut output = String::new();
    let mut nodes = 0_usize;
    let mut truncated = false;
    while let Some((element, depth)) = queue.pop_front() {
        if depth > MAX_UIA_DEPTH
            || nodes >= MAX_UIA_NODES
            || output.len() >= MAX_UIA_BYTES
            || started.elapsed() >= UIA_DEADLINE
        {
            truncated = true;
            break;
        }
        nodes += 1;
        let is_password = unsafe { element.CurrentIsPassword() }.is_ok_and(|value| value.as_bool());
        let role = unsafe { element.CurrentLocalizedControlType() }
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "control".into());
        let name = unsafe { element.CurrentName() }
            .map(|value| clean(&value.to_string()))
            .unwrap_or_default();
        let help = unsafe { element.CurrentHelpText() }
            .map(|value| clean(&value.to_string()))
            .unwrap_or_default();
        let text = if is_password {
            String::new()
        } else {
            element_text(&element).unwrap_or_default()
        };
        let mut fields = Vec::new();
        if !name.is_empty() {
            fields.push(name.clone());
        }
        if !help.is_empty() && help != name {
            fields.push(help);
        }
        if !text.is_empty() && text != name {
            fields.push(text);
        }
        if !fields.is_empty() {
            let line = format!("{}{}: {}\n", "  ".repeat(depth), role, fields.join(" | "));
            if output.len() + line.len() > MAX_UIA_BYTES {
                truncated = true;
                break;
            }
            output.push_str(&line);
        }
        enqueue_children(&walker, &element, depth + 1, &mut queue);
    }
    Ok(AccessibilitySnapshot {
        format_version: 1,
        content: output,
        truncated,
    })
}

fn enqueue_children(
    walker: &IUIAutomationTreeWalker,
    parent: &IUIAutomationElement,
    depth: usize,
    queue: &mut VecDeque<(IUIAutomationElement, usize)>,
) {
    let mut child = unsafe { walker.GetFirstChildElement(parent) }.ok();
    while let Some(element) = child {
        queue.push_back((element.clone(), depth));
        child = unsafe { walker.GetNextSiblingElement(&element) }.ok();
    }
}

fn element_text(element: &IUIAutomationElement) -> Option<String> {
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
    {
        let range = unsafe { pattern.DocumentRange() }.ok()?;
        return unsafe { range.GetText(MAX_UIA_TEXT) }
            .ok()
            .map(|value| clean(&value.to_string()));
    }
    let pattern =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()?;
    unsafe { pattern.CurrentValue() }
        .ok()
        .map(|value| clean(&value.to_string()))
}

fn executable_icon_png(path: &Path) -> Option<Vec<u8>> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = SHFILEINFOW::default();
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    };
    if result == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let png = icon_to_png(info.hIcon).ok();
    let _ = unsafe { DestroyIcon(info.hIcon) };
    png
}

fn icon_to_png(
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> Result<Vec<u8>, CaptureError> {
    const SIZE: i32 = 64;
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return Err(CaptureError::CaptureFailed(
            "Could not create icon drawing context.".into(),
        ));
    }
    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: SIZE,
        biHeight: -SIZE,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(failed)?;
    let previous = unsafe { SelectObject(dc, HGDIOBJ::from(bitmap)) };
    let draw = unsafe { DrawIconEx(dc, 0, 0, icon, SIZE, SIZE, 0, None, DI_NORMAL) };
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    if draw.is_ok() && !bits.is_null() {
        let bgra = unsafe { slice::from_raw_parts(bits.cast::<u8>(), rgba.len()) };
        for (source, target) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            let alpha = if source[3] == 0 && source[..3].iter().any(|value| *value != 0) {
                255
            } else {
                source[3]
            };
            target.copy_from_slice(&[source[2], source[1], source[0], alpha]);
        }
    }
    unsafe {
        SelectObject(dc, previous);
        let _ = DeleteObject(HGDIOBJ::from(bitmap));
        let _ = DeleteDC(dc);
    }
    draw.map_err(failed)?;
    encode_rgba(SIZE as u32, SIZE as u32, &rgba)
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, CaptureError> {
    super::encode_rgba_png(width, height, rgba, "Windows")
}

fn clean(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_UIA_TEXT as usize)
        .collect()
}

fn failed(error: impl std::fmt::Display) -> CaptureError {
    CaptureError::CaptureFailed(format!("Windows Appshot capture failed: {error}"))
}
