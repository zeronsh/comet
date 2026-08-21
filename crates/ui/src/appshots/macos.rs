use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use block::ConcreteBlock;
use chrono::Utc;
use core_foundation::array::CFArray;
use core_foundation::base::{CFRelease, CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::access::ScreenCaptureAccess;
use core_graphics::display::CGRectNull;
use core_graphics::geometry::CGRect;
use core_graphics::window::{
    CGWindowID, copy_window_info, create_image, kCGNullWindowID, kCGWindowBounds,
    kCGWindowImageBestResolution, kCGWindowImageBoundsIgnoreFraming, kCGWindowLayer,
    kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
    kCGWindowListOptionIncludingWindow, kCGWindowName, kCGWindowNumber, kCGWindowOwnerPID,
};
use futures::channel::mpsc;
use objc::rc::autoreleasepool;
use objc::runtime::{Class, Object};
use objc::{class, msg_send, sel, sel_impl};

use super::{AccessibilitySnapshot, CaptureError, CapturedAppshot, PermissionState};
use crate::attachments::{self, StagedAttachment};

const SPACE_KEYCODE: u32 = 49;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;
const EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const EVENT_HOT_KEY_PRESSED: u32 = 5;
const APPSHOT_HOT_KEY_SIGNATURE: u32 = u32::from_be_bytes(*b"ZAPS");
const MAX_AX_DEPTH: usize = 24;
const MAX_AX_NODES: usize = 1_500;
const MAX_AX_BYTES: usize = 96 * 1024;
const MAX_AX_VALUE_CHARS: usize = 4_096;
const AX_DEADLINE: Duration = Duration::from_millis(900);
const SCREEN_CAPTURE_KIT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_CAPTURE_DIMENSION: f64 = 4_096.0;

type AXUIElementRef = *const c_void;
type AXError = i32;
const AX_SUCCESS: AXError = 0;

type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: Option<unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> i32>,
        event_type_count: u32,
        event_types: *const EventTypeSpec,
        user_data: *mut c_void,
        out_handler: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> i32;
}

unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut c_void;
}

struct FrontmostApplication {
    pid: i32,
    name: String,
    bundle_identifier: Option<String>,
    icon_png: Option<Vec<u8>>,
}

struct FrontmostWindow {
    id: CGWindowID,
    title: Option<String>,
}

pub(super) fn permission_state() -> PermissionState {
    PermissionState {
        screen_recording: ScreenCaptureAccess.preflight(),
        accessibility: unsafe { AXIsProcessTrusted() },
    }
}

pub(super) fn request_accessibility_permission() -> PermissionState {
    autoreleasepool(|| unsafe {
        let prompt_value = CFBoolean::true_value();
        let prompt: *mut Object = msg_send![class!(NSDictionary),
            dictionaryWithObject: prompt_value.as_CFTypeRef() as *mut Object
            forKey: kAXTrustedCheckOptionPrompt as *mut Object
        ];
        let _ = AXIsProcessTrustedWithOptions(prompt.cast());
    });
    permission_state()
}

pub(super) fn request_screen_recording_permission() -> PermissionState {
    let _ = ScreenCaptureAccess.request();
    permission_state()
}

pub(super) fn start_global_shortcut() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded();
    // Carbon's global-hotkey API consumes the chord, needs no input-monitoring
    // permission, and keeps working while another app owns keyboard focus.
    let sender = Box::into_raw(Box::new(tx)).cast::<c_void>();
    let event_type = EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    };
    let target = unsafe { GetApplicationEventTarget() };
    let handler_status = unsafe {
        InstallEventHandler(
            target,
            Some(appshot_hot_key_handler),
            1,
            &event_type,
            sender,
            ptr::null_mut(),
        )
    };
    let mut hot_key = ptr::null_mut();
    let register_status = if handler_status == 0 {
        unsafe {
            RegisterEventHotKey(
                SPACE_KEYCODE,
                CONTROL_KEY | OPTION_KEY,
                EventHotKeyId {
                    signature: APPSHOT_HOT_KEY_SIGNATURE,
                    id: 1,
                },
                target,
                0,
                &mut hot_key,
            )
        }
    } else {
        handler_status
    };
    if register_status != 0 {
        tracing::error!(
            status = register_status,
            "Appshot global shortcut unavailable"
        );
        // An installed handler retains this user-data pointer for the process
        // lifetime, even when hotkey registration itself fails.
        if handler_status != 0 {
            unsafe { drop(Box::from_raw(sender.cast::<mpsc::UnboundedSender<()>>())) };
        }
    }
    rx
}

unsafe extern "C" fn appshot_hot_key_handler(
    _call: EventHandlerCallRef,
    _event: EventRef,
    user_data: *mut c_void,
) -> i32 {
    if !user_data.is_null() {
        let sender = unsafe { &*user_data.cast::<mpsc::UnboundedSender<()>>() };
        let _ = sender.unbounded_send(());
    }
    0
}

pub(super) fn capture_frontmost_window() -> Result<CapturedAppshot, CaptureError> {
    autoreleasepool(|| {
        let app = frontmost_application()?;
        if app.pid == std::process::id() as i32 {
            return Err(CaptureError::NoEligibleWindow);
        }
        if !ScreenCaptureAccess.preflight() {
            let state = request_screen_recording_permission();
            return Err(CaptureError::PermissionRequired(state));
        }
        let (window, png) = match capture_with_screen_capture_kit(app.pid) {
            Ok(Some(capture)) => capture,
            Ok(None) => {
                let window = frontmost_window(app.pid)?;
                let png = capture_png(window.id)?;
                (window, png)
            }
            Err(error) => {
                // ScreenCaptureKit is the reliable path for fullscreen Spaces,
                // but keep macOS 12/13 and transient framework failures useful.
                tracing::warn!(%error, "ScreenCaptureKit Appshot failed; using CoreGraphics fallback");
                let window = frontmost_window(app.pid)?;
                let png = capture_png(window.id)?;
                (window, png)
            }
        };
        let screenshot_dimensions = super::png_dimensions(&png);
        let screenshot = stage_png(&app.name, png)?;
        let accessibility = if unsafe { AXIsProcessTrusted() } {
            accessibility_snapshot(app.pid)
        } else {
            AccessibilitySnapshot::unavailable()
        };
        Ok(CapturedAppshot {
            id: uuid::Uuid::new_v4().to_string(),
            app_name: app.name,
            bundle_identifier: app.bundle_identifier,
            window_title: window.title,
            accessibility,
            screenshot,
            screenshot_dimensions,
            app_icon: app.icon_png.map(|bytes| {
                std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes))
            }),
            captured_at: Utc::now(),
        })
    })
}

fn frontmost_application() -> Result<FrontmostApplication, CaptureError> {
    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let app: *mut Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return Err(CaptureError::NoEligibleWindow);
        }
        let pid: i32 = msg_send![app, processIdentifier];
        let name_obj: *mut Object = msg_send![app, localizedName];
        let bundle_obj: *mut Object = msg_send![app, bundleIdentifier];
        let icon: *mut Object = msg_send![app, icon];
        let name = nsstring(name_obj).unwrap_or_else(|| "Application".into());
        Ok(FrontmostApplication {
            pid,
            name,
            bundle_identifier: nsstring(bundle_obj),
            icon_png: nsimage_png(icon),
        })
    }
}

unsafe fn nsimage_png(image: *mut Object) -> Option<Vec<u8>> {
    if image.is_null() {
        return None;
    }
    let tiff: *mut Object = unsafe { msg_send![image, TIFFRepresentation] };
    if tiff.is_null() {
        return None;
    }
    let rep: *mut Object = unsafe { msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff] };
    if rep.is_null() {
        return None;
    }
    let properties: *mut Object = unsafe { msg_send![class!(NSDictionary), dictionary] };
    let data: *mut Object =
        unsafe { msg_send![rep, representationUsingType: 4usize properties: properties] };
    unsafe { nsdata_bytes(data) }
}

unsafe fn nsdata_bytes(data: *mut Object) -> Option<Vec<u8>> {
    if data.is_null() {
        return None;
    }
    let len: usize = unsafe { msg_send![data, length] };
    let bytes: *const u8 = unsafe { msg_send![data, bytes] };
    if bytes.is_null() || len == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec())
}

fn frontmost_window(pid: i32) -> Result<FrontmostWindow, CaptureError> {
    let list = copy_window_info(
        kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )
    .ok_or(CaptureError::NoEligibleWindow)?;
    let mut best: Option<(f64, FrontmostWindow)> = None;
    for item in list.iter() {
        let cf = unsafe { CFType::wrap_under_get_rule(*item as CFTypeRef) };
        let Some(dict) = cf.downcast::<CFDictionary>() else {
            continue;
        };
        let owner = dictionary_number(&dict, unsafe { kCGWindowOwnerPID }).and_then(|n| n.to_i32());
        let layer = dictionary_number(&dict, unsafe { kCGWindowLayer }).and_then(|n| n.to_i32());
        if owner != Some(pid) || layer != Some(0) {
            continue;
        }
        let id = dictionary_number(&dict, unsafe { kCGWindowNumber })
            .and_then(|n| n.to_i64())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(CaptureError::NoEligibleWindow)?;
        let bounds = dictionary_rect(&dict, unsafe { kCGWindowBounds });
        let area = bounds
            .map(|rect| rect.size.width.max(0.0) * rect.size.height.max(0.0))
            .unwrap_or(0.0);
        if area < 1.0 {
            continue;
        }
        let candidate = FrontmostWindow {
            id,
            title: dictionary_string(&dict, unsafe { kCGWindowName }),
        };
        if best.as_ref().is_none_or(|(best_area, _)| area > *best_area) {
            best = Some((area, candidate));
        }
    }
    best.map(|(_, window)| window)
        .ok_or(CaptureError::NoEligibleWindow)
}

fn dictionary_value(dict: &CFDictionary, key: CFStringRef) -> Option<CFType> {
    let value = dict.find(key.cast::<c_void>())?;
    Some(unsafe { CFType::wrap_under_get_rule(*value as CFTypeRef) })
}

fn dictionary_number(dict: &CFDictionary, key: CFStringRef) -> Option<CFNumber> {
    dictionary_value(dict, key)?.downcast::<CFNumber>()
}

fn dictionary_string(dict: &CFDictionary, key: CFStringRef) -> Option<String> {
    dictionary_value(dict, key)?
        .downcast::<CFString>()
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty())
}

fn dictionary_rect(dict: &CFDictionary, key: CFStringRef) -> Option<CGRect> {
    dictionary_value(dict, key)?
        .downcast::<CFDictionary>()
        .and_then(|bounds| CGRect::from_dict_representation(&bounds))
}

/// ScreenCaptureKit is Space-independent and is Apple's supported replacement
/// for the deprecated CGWindowListCreateImage path. It is essential for
/// fullscreen windows, which live in their own Space.
fn capture_with_screen_capture_kit(pid: i32) -> Result<Option<(FrontmostWindow, Vec<u8>)>, String> {
    if !load_screen_capture_kit() {
        return Ok(None);
    }
    let (Some(shareable_class), Some(filter_class), Some(configuration_class), Some(manager_class)) = (
        Class::get("SCShareableContent"),
        Class::get("SCContentFilter"),
        Class::get("SCStreamConfiguration"),
        Class::get("SCScreenshotManager"),
    ) else {
        return Ok(None);
    };

    let (content_tx, content_rx) = std::sync::mpsc::sync_channel(1);
    let content_block = ConcreteBlock::new(move |content: *mut Object, error: *mut Object| {
        let result = unsafe {
            if !content.is_null() {
                let retained: *mut Object = msg_send![content, retain];
                Ok(retained as usize)
            } else {
                Err(ns_error_message(
                    error,
                    "ScreenCaptureKit could not enumerate windows",
                ))
            }
        };
        if let Err(unsent) = content_tx.send(result)
            && let Ok(content) = unsent.0
        {
            unsafe { CFRelease((content as *const c_void).cast()) };
        }
    })
    .copy();
    unsafe {
        let _: () = msg_send![
            shareable_class,
            getShareableContentExcludingDesktopWindows: 1i8
            onScreenWindowsOnly: 0i8
            completionHandler: &*content_block
        ];
    }
    let content = content_rx
        .recv_timeout(SCREEN_CAPTURE_KIT_TIMEOUT)
        .map_err(|_| "Timed out while enumerating capturable windows".to_string())??
        as *mut Object;

    let selected = unsafe { select_screen_capture_kit_window(content, pid) };
    unsafe {
        let _: () = msg_send![content, release];
    }
    let Some((window, metadata, points_wide, points_high)) = selected else {
        return Err("The frontmost application has no ScreenCaptureKit window".into());
    };

    let (pixel_width, pixel_height) = capture_dimensions(points_wide, points_high);
    let filter: *mut Object = unsafe { msg_send![filter_class, alloc] };
    let filter: *mut Object =
        unsafe { msg_send![filter, initWithDesktopIndependentWindow: window] };
    let configuration: *mut Object = unsafe { msg_send![configuration_class, new] };
    unsafe {
        let _: () = msg_send![configuration, setWidth: pixel_width];
        let _: () = msg_send![configuration, setHeight: pixel_height];
        let _: () = msg_send![configuration, setScalesToFit: 1i8];
        let _: () = msg_send![configuration, setPreservesAspectRatio: 1i8];
        let _: () = msg_send![configuration, setShowsCursor: 0i8];
        let _: () = msg_send![configuration, setIgnoreShadowsSingleWindow: 1i8];
    }

    let (image_tx, image_rx) = std::sync::mpsc::sync_channel(1);
    let image_block = ConcreteBlock::new(move |image: *mut c_void, error: *mut Object| {
        let result = unsafe {
            if !image.is_null() {
                // The completion owns the image only for this call. A CGImage
                // is a CF object, so retain it before crossing the channel.
                core_foundation::base::CFRetain(image.cast()) as usize
            } else {
                let _ = image_tx.send(Err(ns_error_message(
                    error,
                    "ScreenCaptureKit returned no screenshot",
                )));
                return;
            }
        };
        if image_tx.send(Ok(result)).is_err() {
            unsafe { CFRelease((result as *const c_void).cast()) };
        }
    })
    .copy();
    unsafe {
        let _: () = msg_send![
            manager_class,
            captureImageWithFilter: filter
            configuration: configuration
            completionHandler: &*image_block
        ];
    }
    let image_result = image_rx
        .recv_timeout(SCREEN_CAPTURE_KIT_TIMEOUT)
        .map_err(|_| "Timed out while capturing the frontmost window".to_string());
    unsafe {
        let _: () = msg_send![configuration, release];
        let _: () = msg_send![filter, release];
        let _: () = msg_send![window, release];
    }
    let image = image_result?? as *mut c_void;
    let png = encode_png(image);
    unsafe {
        CFRelease(image.cast());
    }
    png.map(|png| Some((metadata, png)))
}

fn load_screen_capture_kit() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| unsafe {
        // Load dynamically so Zeron's macOS 12.0 minimum remains valid. The
        // screenshot manager arrived in macOS 14; older systems use fallback.
        let path = b"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit\0";
        !dlopen(path.as_ptr().cast(), 1).is_null() // RTLD_LAZY
    })
}

/// Returns a retained SCWindow plus metadata and its size in screen points.
unsafe fn select_screen_capture_kit_window(
    content: *mut Object,
    pid: i32,
) -> Option<(*mut Object, FrontmostWindow, f64, f64)> {
    let windows: *mut Object = unsafe { msg_send![content, windows] };
    let count: usize = unsafe { msg_send![windows, count] };
    let mut best: Option<(u8, f64, *mut Object, FrontmostWindow, f64, f64)> = None;
    for index in 0..count {
        let window: *mut Object = unsafe { msg_send![windows, objectAtIndex: index] };
        let owner: *mut Object = unsafe { msg_send![window, owningApplication] };
        if owner.is_null() {
            continue;
        }
        let owner_pid: i32 = unsafe { msg_send![owner, processID] };
        let layer: isize = unsafe { msg_send![window, windowLayer] };
        if owner_pid != pid || layer != 0 {
            continue;
        }
        let id: CGWindowID = unsafe { msg_send![window, windowID] };
        let bounds = window_bounds(id);
        let (width, height, area) = bounds
            .map(|rect| {
                let width = rect.size.width.max(1.0);
                let height = rect.size.height.max(1.0);
                (width, height, width * height)
            })
            .unwrap_or((1_920.0, 1_080.0, 1.0));
        let active: i8 = unsafe { msg_send![window, isActive] };
        let on_screen: i8 = unsafe { msg_send![window, isOnScreen] };
        let rank = (active != 0) as u8 * 2 + (on_screen != 0) as u8;
        let title_obj: *mut Object = unsafe { msg_send![window, title] };
        let metadata = FrontmostWindow {
            id,
            title: unsafe { nsstring(title_obj) },
        };
        if best.as_ref().is_none_or(|(best_rank, best_area, ..)| {
            rank > *best_rank || (rank == *best_rank && area > *best_area)
        }) {
            best = Some((rank, area, window, metadata, width, height));
        }
    }
    best.map(|(_, _, window, metadata, width, height)| {
        let retained: *mut Object = unsafe { msg_send![window, retain] };
        (retained, metadata, width, height)
    })
}

fn window_bounds(window_id: CGWindowID) -> Option<CGRect> {
    let list = copy_window_info(kCGWindowListOptionIncludingWindow, window_id)?;
    let item = *list.iter().next()?;
    let cf = unsafe { CFType::wrap_under_get_rule(item as CFTypeRef) };
    let dict = cf.downcast::<CFDictionary>()?;
    dictionary_rect(&dict, unsafe { kCGWindowBounds })
}

fn capture_dimensions(width: f64, height: f64) -> (usize, usize) {
    let longest = width.max(height).max(1.0);
    let scale = 2.0f64.min(MAX_CAPTURE_DIMENSION / longest);
    (
        (width * scale).round().max(1.0) as usize,
        (height * scale).round().max(1.0) as usize,
    )
}

unsafe fn ns_error_message(error: *mut Object, fallback: &str) -> String {
    if error.is_null() {
        return fallback.into();
    }
    let description: *mut Object = unsafe { msg_send![error, localizedDescription] };
    unsafe { nsstring(description) }.unwrap_or_else(|| fallback.into())
}

fn capture_png(window_id: CGWindowID) -> Result<Vec<u8>, CaptureError> {
    let image = create_image(
        unsafe { CGRectNull },
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageBestResolution | kCGWindowImageBoundsIgnoreFraming,
    )
    .ok_or_else(|| {
        CaptureError::CaptureFailed("The application window could not be captured.".into())
    })?;
    use foreign_types::ForeignType;
    encode_png(image.as_ptr().cast()).map_err(CaptureError::CaptureFailed)
}

fn encode_png(image: *mut c_void) -> Result<Vec<u8>, String> {
    unsafe {
        let rep: *mut Object = msg_send![class!(NSBitmapImageRep), alloc];
        let rep: *mut Object = msg_send![rep, initWithCGImage: image];
        if rep.is_null() {
            return Err("The screenshot could not be encoded.".into());
        }
        let properties: *mut Object = msg_send![class!(NSDictionary), dictionary];
        // NSBitmapImageFileTypePNG = 4.
        let data: *mut Object =
            msg_send![rep, representationUsingType: 4usize properties: properties];
        let _: () = msg_send![rep, release];
        if data.is_null() {
            return Err("The screenshot could not be encoded.".into());
        }
        let len: usize = msg_send![data, length];
        let bytes: *const u8 = msg_send![data, bytes];
        if bytes.is_null() || len == 0 {
            return Err("The captured window was empty.".into());
        }
        Ok(std::slice::from_raw_parts(bytes, len).to_vec())
    }
}

fn stage_png(app_name: &str, bytes: Vec<u8>) -> Result<StagedAttachment, CaptureError> {
    if bytes.len() as u64 > attachments::MAX_ATTACHMENT_BYTES {
        return Err(CaptureError::CaptureFailed(
            "The captured window is larger than Zeron's 24 MB image limit.".into(),
        ));
    }
    let safe_name: String = app_name
        .chars()
        .map(|ch| if ch == '/' || ch == ':' { '-' } else { ch })
        .collect();
    Ok(attachments::stage_png_bytes(
        format!("{safe_name} Appshot.png"),
        bytes,
    ))
}

fn accessibility_snapshot(pid: i32) -> AccessibilitySnapshot {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return AccessibilitySnapshot::unavailable();
        }
        let focused = copy_ax_value(app, "AXFocusedWindow");
        let root = focused
            .as_ref()
            .map(|value| value.as_CFTypeRef() as AXUIElementRef)
            .unwrap_or(app);
        let mut state = AxTraversal::new();
        state.visit(root, 0);
        CFRelease(app.cast());
        AccessibilitySnapshot {
            format_version: 1,
            content: state.output,
            truncated: state.truncated,
        }
    }
}

struct AxTraversal {
    output: String,
    nodes: usize,
    truncated: bool,
    deadline: Instant,
}

impl AxTraversal {
    fn new() -> Self {
        Self {
            output: String::new(),
            nodes: 0,
            truncated: false,
            deadline: Instant::now() + AX_DEADLINE,
        }
    }

    unsafe fn visit(&mut self, element: AXUIElementRef, depth: usize) {
        if depth > MAX_AX_DEPTH
            || self.nodes >= MAX_AX_NODES
            || self.output.len() >= MAX_AX_BYTES
            || Instant::now() >= self.deadline
        {
            self.truncated = true;
            return;
        }
        self.nodes += 1;
        let role = unsafe { ax_string(element, "AXRole") }.unwrap_or_else(|| "AXElement".into());
        let title = unsafe { ax_string(element, "AXTitle") };
        let description = unsafe { ax_string(element, "AXDescription") };
        let value = if role == "AXSecureTextField" {
            None
        } else {
            unsafe { ax_scalar_string(element, "AXValue") }
        };
        let mut fields = Vec::new();
        if let Some(title) = title.filter(|s| !s.trim().is_empty()) {
            fields.push(format!("title={}", compact(&title)));
        }
        if let Some(description) = description.filter(|s| !s.trim().is_empty()) {
            fields.push(format!("description={}", compact(&description)));
        }
        if let Some(value) = value.filter(|s| !s.trim().is_empty()) {
            fields.push(format!("value={}", compact(&value)));
        }
        let line = if fields.is_empty() {
            format!("{}{}\n", "  ".repeat(depth), role)
        } else {
            format!("{}{} {}\n", "  ".repeat(depth), role, fields.join(" "))
        };
        let remaining = MAX_AX_BYTES.saturating_sub(self.output.len());
        if line.len() > remaining {
            let end = line
                .char_indices()
                .take_while(|(index, _)| *index <= remaining)
                .map(|(index, _)| index)
                .last()
                .unwrap_or(0);
            self.output.push_str(&line[..end]);
            self.truncated = true;
            return;
        }
        self.output.push_str(&line);
        if role == "AXSecureTextField" {
            return;
        }
        let Some(children_value) = (unsafe { copy_ax_value(element, "AXChildren") }) else {
            return;
        };
        if let Some(children) = children_value.downcast::<CFArray>() {
            for child in children.iter() {
                unsafe { self.visit(*child as AXUIElementRef, depth + 1) };
                if self.truncated {
                    break;
                }
            }
        }
    }
}

unsafe fn copy_ax_value(element: AXUIElementRef, attribute: &str) -> Option<CFType> {
    let attribute = CFString::new(attribute);
    let mut value: CFTypeRef = ptr::null();
    if unsafe {
        AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
    } != AX_SUCCESS
        || value.is_null()
    {
        return None;
    }
    Some(unsafe { CFType::wrap_under_create_rule(value) })
}

unsafe fn ax_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
    unsafe { copy_ax_value(element, attribute) }?
        .downcast::<CFString>()
        .map(|value| value.to_string())
}

unsafe fn ax_scalar_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
    let value = unsafe { copy_ax_value(element, attribute) }?;
    if let Some(string) = value.downcast::<CFString>() {
        return Some(string.to_string());
    }
    if let Some(number) = value.downcast::<CFNumber>() {
        return number
            .to_i64()
            .map(|number| number.to_string())
            .or_else(|| number.to_f64().map(|number| number.to_string()));
    }
    None
}

fn compact(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_AX_VALUE_CHARS {
        compact
    } else {
        let end = compact
            .char_indices()
            .nth(MAX_AX_VALUE_CHARS)
            .map(|(index, _)| index)
            .unwrap_or(compact.len());
        format!("{}…", &compact[..end])
    }
}

unsafe fn nsstring(value: *mut Object) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let bytes: *const std::ffi::c_char = msg_send![value, UTF8String];
    (!bytes.is_null()).then(|| {
        unsafe { CStr::from_ptr(bytes) }
            .to_string_lossy()
            .into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::capture_dimensions;

    #[test]
    fn capture_dimensions_use_retina_scale_for_ordinary_windows() {
        assert_eq!(capture_dimensions(1_440.0, 900.0), (2_880, 1_800));
    }

    #[test]
    fn capture_dimensions_cap_large_fullscreen_windows() {
        assert_eq!(capture_dimensions(5_120.0, 2_880.0), (4_096, 2_304));
    }
}
