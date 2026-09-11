use std::cell::RefCell;
use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
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
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    CGWindowID, copy_window_info, create_image, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds,
    kCGWindowImageBoundsIgnoreFraming, kCGWindowImageNominalResolution, kCGWindowIsOnscreen,
    kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionAll,
    kCGWindowListOptionIncludingWindow, kCGWindowName, kCGWindowNumber, kCGWindowOwnerPID,
};
use futures::channel::mpsc;
use objc::rc::autoreleasepool;
use objc::runtime::{Class, Object};
use objc::{class, msg_send, sel, sel_impl};

use super::{
    ACCESSIBILITY_SETTINGS_URL, AccessibilitySnapshot, AppshotBackend, AppshotCapabilities,
    AppshotPlatform, CapabilityState, CaptureError, CaptureTarget, CapturedAppshot,
    SCREEN_RECORDING_SETTINGS_URL,
};

const COMMAND_KEY: u32 = 1 << 8;
const SHIFT_KEY: u32 = 1 << 9;
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
static SHORTCUT_READY: AtomicBool = AtomicBool::new(true);

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
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, seconds: f32) -> AXError;
    fn AXValueGetTypeID() -> core_foundation::base::CFTypeID;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, result: *mut c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> CFTypeRef;
    fn TISGetInputSourceProperty(source: CFTypeRef, property: CFStringRef) -> *const c_void;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    fn UCKeyTranslate(
        layout: *const c_void,
        key_code: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: usize,
        actual_length: *mut usize,
        output: *mut u16,
    ) -> i32;
    fn LMGetKbdType() -> u16;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> i32;
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

pub struct MacOsBackend;

#[async_trait::async_trait]
impl AppshotBackend for MacOsBackend {
    fn capabilities(&self) -> AppshotCapabilities {
        AppshotCapabilities {
            platform: AppshotPlatform::MacOs,
            global_shortcut: if SHORTCUT_READY.load(Ordering::Relaxed) {
                CapabilityState::Ready
            } else {
                CapabilityState::SetupRequired
            },
            window_capture: if ScreenCaptureAccess.preflight() {
                CapabilityState::Ready
            } else {
                CapabilityState::PermissionRequired
            },
            application_text: if unsafe { AXIsProcessTrusted() } {
                CapabilityState::Ready
            } else {
                CapabilityState::PermissionRequired
            },
            target: CaptureTarget::ActiveWindow,
        }
    }

    fn start_global_shortcut(
        &self,
        _activation_dir: &std::path::Path,
    ) -> mpsc::UnboundedReceiver<()> {
        start_global_shortcut()
    }

    async fn capture_active_window(&self) -> Result<CapturedAppshot, CaptureError> {
        capture_frontmost_window()
    }

    fn request_capture_access(&self) {
        request_screen_recording_permission();
    }

    fn request_semantic_access(&self) {
        request_accessibility_permission();
    }

    fn capture_settings_url(&self) -> Option<&'static str> {
        Some(SCREEN_RECORDING_SETTINGS_URL)
    }

    fn semantic_settings_url(&self) -> Option<&'static str> {
        Some(ACCESSIBILITY_SETTINGS_URL)
    }
}

fn request_accessibility_permission() {
    autoreleasepool(|| unsafe {
        let prompt_value = CFBoolean::true_value();
        let prompt: *mut Object = msg_send![class!(NSDictionary),
            dictionaryWithObject: prompt_value.as_CFTypeRef() as *mut Object
            forKey: kAXTrustedCheckOptionPrompt as *mut Object
        ];
        let _ = AXIsProcessTrustedWithOptions(prompt.cast());
    });
}

fn request_screen_recording_permission() {
    let _ = ScreenCaptureAccess.request();
}

struct HotKeyRegistration {
    target: EventTargetRef,
    hot_key: EventHotKeyRef,
    shortcut: Option<super::shortcut::Shortcut>,
}

thread_local! {
    // Carbon registration and replacement always run on GPUI's main thread.
    static HOT_KEY: RefCell<Option<HotKeyRegistration>> = const { RefCell::new(None) };
}

fn start_global_shortcut() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded();
    let sender = Box::into_raw(Box::new(tx)).cast::<c_void>();
    let event_type = EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    };
    let target = unsafe { GetApplicationEventTarget() };
    let status = unsafe {
        InstallEventHandler(
            target,
            Some(appshot_hot_key_handler),
            1,
            &event_type,
            sender,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        unsafe { drop(Box::from_raw(sender.cast::<mpsc::UnboundedSender<()>>())) };
        SHORTCUT_READY.store(false, Ordering::Relaxed);
        tracing::warn!(status, "Appshot global shortcut handler unavailable");
        return rx;
    }
    HOT_KEY.with(|registration| {
        *registration.borrow_mut() = Some(HotKeyRegistration {
            target,
            hot_key: ptr::null_mut(),
            shortcut: None,
        })
    });
    refresh_global_shortcut(super::shortcut::current());
    rx
}

pub(super) fn refresh_global_shortcut(shortcut: Option<super::shortcut::Shortcut>) {
    HOT_KEY.with(|registration| {
        let mut registration = registration.borrow_mut();
        let Some(registration) = registration.as_mut() else {
            return;
        };
        if registration.shortcut == shortcut {
            return;
        }
        if !registration.hot_key.is_null() {
            unsafe {
                UnregisterEventHotKey(registration.hot_key);
            }
            registration.hot_key = ptr::null_mut();
        }
        registration.shortcut = shortcut.clone();
        let Some(shortcut) = shortcut else {
            return;
        };
        let Some(key_code) = shortcut_keycode(&shortcut.key) else {
            SHORTCUT_READY.store(false, Ordering::Relaxed);
            return;
        };
        let modifiers = (if shortcut.control { CONTROL_KEY } else { 0 })
            | (if shortcut.alt { OPTION_KEY } else { 0 })
            | (if shortcut.shift { SHIFT_KEY } else { 0 })
            | (if shortcut.platform { COMMAND_KEY } else { 0 });
        let status = unsafe {
            RegisterEventHotKey(
                key_code,
                modifiers,
                EventHotKeyId {
                    signature: APPSHOT_HOT_KEY_SIGNATURE,
                    id: 1,
                },
                registration.target,
                0,
                &mut registration.hot_key,
            )
        };
        SHORTCUT_READY.store(status == 0, Ordering::Relaxed);
        if status != 0 {
            tracing::warn!(status, "Appshot global shortcut unavailable");
        }
    });
}

fn shortcut_keycode(key: &str) -> Option<u32> {
    let named = match key {
        "space" => 49,
        "tab" => 48,
        "enter" => 36,
        "backspace" => 51,
        "delete" => 117,
        "insert" => 114,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        _ => {
            if let Some(index) = key.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
                return index.checked_sub(1).and_then(|index| {
                    [
                        122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106,
                        64, 79, 80, 90,
                    ]
                    .get(index)
                    .copied()
                });
            }
            return layout_keycode(key);
        }
    };
    Some(named)
}

fn layout_keycode(key: &str) -> Option<u32> {
    // Resolve the printed key against the active ASCII-capable layout. A US
    // positional table would capture Q when an AZERTY user records A.
    unsafe {
        let source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
        if source.is_null() {
            return None;
        }
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        let result = if data.is_null() {
            None
        } else {
            let layout = core_foundation::data::CFDataGetBytePtr(data.cast()).cast();
            let mut found = None;
            'levels: for modifiers in [0, 2] {
                for code in 0..128_u16 {
                    let mut output = [0_u16; 4];
                    let mut length = 0;
                    let mut dead = 0;
                    if UCKeyTranslate(
                        layout,
                        code,
                        0,
                        modifiers,
                        u32::from(LMGetKbdType()),
                        1,
                        &mut dead,
                        output.len(),
                        &mut length,
                        output.as_mut_ptr(),
                    ) == 0
                        && length <= output.len()
                        && String::from_utf16_lossy(&output[..length]).to_lowercase() == key
                    {
                        found = Some(u32::from(code));
                        break 'levels;
                    }
                }
            }
            found
        };
        CFRelease(source);
        result
    }
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

fn capture_frontmost_window() -> Result<CapturedAppshot, CaptureError> {
    autoreleasepool(|| {
        let started = Instant::now();
        let app = frontmost_application()?;
        if app.pid == std::process::id() as i32 {
            return Err(CaptureError::SelfCapture);
        }
        if !ScreenCaptureAccess.preflight() {
            request_screen_recording_permission();
            return Err(CaptureError::PermissionRequired);
        }
        // Resolve the actual front-to-back window once, before asynchronous
        // ScreenCaptureKit work can allow focus to change. Every capture path
        // remains bound to this exact CGWindowID.
        let preserved_window = frontmost_window(app.pid)?;
        // Retain the AX window before asynchronous capture. Never resolve focus
        // again afterward: another document may have become active by then.
        let accessibility_window = preserved_accessibility_window(app.pid, preserved_window.id);
        let (window, png) = match capture_with_screen_capture_kit(app.pid, preserved_window.id) {
            Ok(Some(capture)) => capture,
            Ok(None) => {
                let png = capture_png(preserved_window.id)?;
                (preserved_window, png)
            }
            Err(error) => {
                // ScreenCaptureKit is the reliable path for fullscreen Spaces,
                // but keep macOS 12/13 and transient framework failures useful.
                tracing::warn!(%error, "ScreenCaptureKit Appshot failed; using CoreGraphics fallback");
                let png = capture_png(preserved_window.id)?;
                (preserved_window, png)
            }
        };
        let pixels_ready = Instant::now();
        let (screenshot, screenshot_dimensions) = super::stage_appshot_png(&app.name, png)?;
        super::capture_ready();
        tracing::debug!(
            capture_ms = pixels_ready.duration_since(started).as_millis(),
            staging_ms = pixels_ready.elapsed().as_millis(),
            total_ms = started.elapsed().as_millis(),
            "Appshot capture feedback requested"
        );
        let accessibility = accessibility_window
            .as_ref()
            .map(accessibility_snapshot)
            .unwrap_or_else(AccessibilitySnapshot::unavailable);
        Ok(CapturedAppshot {
            id: uuid::Uuid::new_v4().to_string(),
            app_name: app.name,
            bundle_identifier: app.bundle_identifier,
            window_title: window.title,
            accessibility,
            screenshot,
            screenshot_dimensions: Some(screenshot_dimensions),
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

pub(super) fn icon_for_bundle(bundle: &str) -> Option<Vec<u8>> {
    autoreleasepool(|| unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let bundle = CFString::new(bundle);
        let url: *mut Object = msg_send![workspace, URLForApplicationWithBundleIdentifier: bundle.as_concrete_TypeRef()];
        if url.is_null() {
            return None;
        }
        let path: *mut Object = msg_send![url, path];
        let icon: *mut Object = msg_send![workspace, iconForFile: path];
        nsimage_png(icon)
    })
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
    let focused_bounds = focused_window_bounds(pid);
    // CGWindowList is front-to-back. The first eligible layer-zero window is
    // the frontmost visible one. Chromium and GPUI apps also own hidden,
    // layer-zero helper windows; those are not capture targets. Retain the
    // all-Spaces enumeration for ScreenCaptureKit, but exclude offscreen and
    // transparent windows before binding the capture to a stable window ID.
    // Choosing by area could select an unrelated background document.
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
        let onscreen = dictionary_value(&dict, unsafe { kCGWindowIsOnscreen })
            .and_then(|value| value.downcast::<CFBoolean>())
            .is_some_and(|value| value == CFBoolean::true_value());
        let alpha = dictionary_number(&dict, unsafe { kCGWindowAlpha })
            .and_then(|value| value.to_f64())
            .unwrap_or(0.0);
        if !visible_capture_window(onscreen, alpha, bounds) {
            continue;
        }
        let Some(bounds) = bounds else { continue };
        if focused_bounds.is_some_and(|focused| !same_window_bounds(bounds, focused)) {
            continue;
        }
        tracing::debug!(
            window_id = id,
            width = bounds.size.width,
            height = bounds.size.height,
            matched_focus = focused_bounds.is_some(),
            "Appshot capture target selected"
        );
        let candidate = FrontmostWindow {
            id,
            title: dictionary_string(&dict, unsafe { kCGWindowName }),
        };
        return Ok(candidate);
    }
    Err(CaptureError::NoEligibleWindow)
}

fn visible_capture_window(onscreen: bool, alpha: f64, bounds: Option<CGRect>) -> bool {
    onscreen
        && alpha.is_finite()
        && alpha > 0.0
        && bounds.is_some_and(|bounds| {
            bounds.size.width.is_finite()
                && bounds.size.height.is_finite()
                && bounds.size.width >= 32.0
                && bounds.size.height >= 32.0
        })
}

/// Match the actual focused document instead of a Chromium helper that can be
/// layer zero, on screen, and fully opaque while still containing no UI.
/// Both AX and WindowServer rectangles use global top-left coordinates.
fn focused_window_bounds(pid: i32) -> Option<CGRect> {
    if !unsafe { AXIsProcessTrusted() } {
        return None;
    }
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        let focused = copy_ax_value(app, "AXFocusedWindow", deadline);
        CFRelease(app.cast());
        let focused = focused?;
        let element = focused.as_CFTypeRef() as AXUIElementRef;
        let position = copy_ax_value(element, "AXPosition", deadline)?;
        let size = copy_ax_value(element, "AXSize", deadline)?;
        if position.type_of() != AXValueGetTypeID() || size.type_of() != AXValueGetTypeID() {
            return None;
        }
        let mut origin = CGPoint::new(0.0, 0.0);
        let mut dimensions = CGSize::new(0.0, 0.0);
        if !AXValueGetValue(
            position.as_CFTypeRef(),
            1,
            (&mut origin as *mut CGPoint).cast(),
        ) || !AXValueGetValue(
            size.as_CFTypeRef(),
            2,
            (&mut dimensions as *mut CGSize).cast(),
        ) {
            return None;
        }
        let bounds = CGRect::new(&origin, &dimensions);
        visible_capture_window(true, 1.0, Some(bounds)).then_some(bounds)
    }
}

fn same_window_bounds(window: CGRect, focused: CGRect) -> bool {
    // Allow WindowServer/AX rounding at fractional display scales.
    [
        (window.origin.x, focused.origin.x),
        (window.origin.y, focused.origin.y),
        (window.size.width, focused.size.width),
        (window.size.height, focused.size.height),
    ]
    .iter()
    .all(|(a, b)| (a - b).abs() <= 2.0)
}

/// Use LaunchServices to bring this app back after the explicit capture
/// shortcut. GPUI's deprecated activateIgnoringOtherApps call is only a
/// cooperative request on modern macOS and can leave the source app in front.
/// Run on the main thread, after capture has finished and the tile is staged.
pub(super) fn foreground_after_capture() {
    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let bundle: *mut Object = msg_send![class!(NSBundle), mainBundle];
        let url: *mut Object = msg_send![bundle, bundleURL];
        let extension: *mut Object = msg_send![url, pathExtension];
        if nsstring(extension).as_deref() != Some("app") {
            // A bare `cargo run` has no launchable app bundle. GPUI's normal
            // window activation above remains the fallback for that case.
            return;
        }
        let configuration: *mut Object =
            msg_send![class!(NSWorkspaceOpenConfiguration), configuration];
        let _: () = msg_send![configuration, setActivates: 1i8];
        let _: () = msg_send![configuration, setCreatesNewApplicationInstance: 0i8];
        let completion = ConcreteBlock::new(|_app: *mut Object, error: *mut Object| {
            if !error.is_null() {
                tracing::warn!("Could not bring Zeron forward after Appshot capture");
            }
        })
        .copy();
        let _: () = msg_send![workspace, openApplicationAtURL: url configuration: configuration completionHandler: &*completion];
    }
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
fn capture_with_screen_capture_kit(
    pid: i32,
    preserved_window_id: CGWindowID,
) -> Result<Option<(FrontmostWindow, Vec<u8>)>, String> {
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

    let selected = unsafe { select_screen_capture_kit_window(content, pid, preserved_window_id) };
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
    preserved_window_id: CGWindowID,
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
        if id != preserved_window_id {
            continue;
        }
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

fn fallback_capture_dimensions(bounds: CGRect) -> Result<(u32, u32), CaptureError> {
    if !bounds.origin.x.is_finite()
        || !bounds.origin.y.is_finite()
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
        || bounds.size.width > u32::MAX as f64
        || bounds.size.height > u32::MAX as f64
    {
        return Err(CaptureError::NoEligibleWindow);
    }
    // Nominal resolution requests one pixel per point. Reject oversized
    // rectangles before asking WindowServer to allocate the capture.
    let dimensions = (
        bounds.size.width.ceil() as u32,
        bounds.size.height.ceil() as u32,
    );
    super::validate_capture_dimensions(dimensions.0, dimensions.1)?;
    Ok(dimensions)
}

fn capture_png(window_id: CGWindowID) -> Result<Vec<u8>, CaptureError> {
    let bounds = window_bounds(window_id).ok_or(CaptureError::NoEligibleWindow)?;
    fallback_capture_dimensions(bounds)?;
    let image = create_image(
        bounds,
        kCGWindowListOptionIncludingWindow,
        window_id,
        kCGWindowImageNominalResolution | kCGWindowImageBoundsIgnoreFraming,
    )
    .ok_or_else(|| {
        CaptureError::CaptureFailed("The application window could not be captured.".into())
    })?;
    use foreign_types::ForeignType;
    encode_png(image.as_ptr().cast()).map_err(CaptureError::CaptureFailed)
}

fn encode_png(image: *mut c_void) -> Result<Vec<u8>, String> {
    unsafe extern "C" {
        fn CGImageGetWidth(image: *mut c_void) -> usize;
        fn CGImageGetHeight(image: *mut c_void) -> usize;
    }
    if image.is_null() {
        return Err("The captured window was empty.".into());
    }
    let width = u32::try_from(unsafe { CGImageGetWidth(image) })
        .map_err(|_| "Screenshot width exceeds the capture budget")?;
    let height = u32::try_from(unsafe { CGImageGetHeight(image) })
        .map_err(|_| "Screenshot height exceeds the capture budget")?;
    super::validate_capture_dimensions(width, height).map_err(|error| error.to_string())?;
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
        if len as u64 > crate::attachments::MAX_ATTACHMENT_BYTES {
            return Err("The captured window is larger than Zeron's 24 MB image limit.".into());
        }
        let bytes: *const u8 = msg_send![data, bytes];
        if bytes.is_null() || len == 0 {
            return Err("The captured window was empty.".into());
        }
        Ok(std::slice::from_raw_parts(bytes, len).to_vec())
    }
}

fn preserved_accessibility_window(pid: i32, window_id: CGWindowID) -> Option<CFType> {
    if !unsafe { AXIsProcessTrusted() } {
        return None;
    }
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        let focused = copy_ax_value(app, "AXFocusedWindow", deadline);
        CFRelease(app.cast());
        let focused = focused?;
        // Resolve the CG identity while this AX element is still focused. If
        // focus moved during acquisition, omit semantics instead of guessing.
        if frontmost_window(pid).ok()?.id != window_id {
            return None;
        }
        Some(focused)
    }
}

fn accessibility_snapshot(window: &CFType) -> AccessibilitySnapshot {
    let mut state = AxTraversal::new();
    unsafe { state.visit(window.as_CFTypeRef() as AXUIElementRef, 0) };
    AccessibilitySnapshot {
        format_version: 1,
        content: state.output,
        truncated: state.truncated,
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
        let role = unsafe { ax_string(element, "AXRole", self.deadline) }
            .unwrap_or_else(|| "AXElement".into());
        let subrole = unsafe { ax_string(element, "AXSubrole", self.deadline) };
        let secure = is_secure_ax_element(&role, subrole.as_deref());
        let title = unsafe { ax_string(element, "AXTitle", self.deadline) };
        let description = unsafe { ax_string(element, "AXDescription", self.deadline) };
        let value = if secure {
            None
        } else {
            unsafe { ax_scalar_string(element, "AXValue", self.deadline) }
        };
        if Instant::now() >= self.deadline {
            self.truncated = true;
            return;
        }
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
        if secure {
            return;
        }
        let Some(children_value) = (unsafe { copy_ax_value(element, "AXChildren", self.deadline) })
        else {
            self.truncated |= Instant::now() >= self.deadline;
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

fn is_secure_ax_element(role: &str, subrole: Option<&str>) -> bool {
    role == "AXSecureTextField" || subrole == Some("AXSecureTextField")
}

unsafe fn copy_ax_value(
    element: AXUIElementRef,
    attribute: &str,
    deadline: Instant,
) -> Option<CFType> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    if remaining.is_zero()
        || unsafe { AXUIElementSetMessagingTimeout(element, remaining.as_secs_f32().min(0.25)) }
            != AX_SUCCESS
    {
        return None;
    }
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

unsafe fn ax_string(element: AXUIElementRef, attribute: &str, deadline: Instant) -> Option<String> {
    unsafe { copy_ax_value(element, attribute, deadline) }?
        .downcast::<CFString>()
        .map(|value| value.to_string())
}

unsafe fn ax_scalar_string(
    element: AXUIElementRef,
    attribute: &str,
    deadline: Instant,
) -> Option<String> {
    let value = unsafe { copy_ax_value(element, attribute, deadline) }?;
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
    use super::{
        capture_dimensions, is_secure_ax_element, same_window_bounds, visible_capture_window,
    };
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};

    #[test]
    fn fallback_rejects_oversized_or_invalid_native_acquisition() {
        let bounds = |w, h| CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(w, h));
        assert_eq!(
            super::fallback_capture_dimensions(bounds(1440.2, 900.0)).unwrap(),
            (1441, 900)
        );
        for (w, h) in [
            (9000.0, 500.0),
            (8192.0, 8192.0),
            (f64::NAN, 500.0),
            (100.0, 0.0),
        ] {
            assert!(super::fallback_capture_dimensions(bounds(w, h)).is_err());
        }
    }

    #[test]
    fn expired_ax_budget_never_calls_the_native_element() {
        // A null element is safe here only because the elapsed budget must
        // return before any native AX call. It guards the deadline boundary.
        assert!(
            unsafe { super::copy_ax_value(std::ptr::null(), "AXTitle", std::time::Instant::now()) }
                .is_none()
        );
    }

    #[test]
    fn focused_document_wins_over_onscreen_helpers_and_other_documents() {
        let focused = CGRect::new(&CGPoint::new(80.0, 40.0), &CGSize::new(1000.0, 700.0));
        let helper = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(128.0, 128.0));
        let background = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(1400.0, 900.0));
        let candidates = [helper, background, focused];
        assert_eq!(
            candidates
                .iter()
                .position(|bounds| same_window_bounds(*bounds, focused)),
            Some(2)
        );
        assert!(!visible_capture_window(
            true,
            1.0,
            Some(CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(1.0, 1.0)))
        ));
    }

    #[test]
    fn capture_skips_hidden_helpers_without_rejecting_small_visible_windows() {
        let bounds = Some(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(200.0, 120.0),
        ));
        assert!(!visible_capture_window(false, 1.0, bounds));
        assert!(!visible_capture_window(true, 0.0, bounds));
        assert!(!visible_capture_window(true, f64::NAN, bounds));
        assert!(!visible_capture_window(true, 1.0, None));
        assert!(visible_capture_window(true, 1.0, bounds));
    }

    #[test]
    fn capture_dimensions_use_retina_scale_for_ordinary_windows() {
        assert_eq!(capture_dimensions(1_440.0, 900.0), (2_880, 1_800));
    }

    #[test]
    fn capture_dimensions_cap_large_fullscreen_windows() {
        assert_eq!(capture_dimensions(5_120.0, 2_880.0), (4_096, 2_304));
    }

    #[test]
    fn secure_text_subroles_are_redacted() {
        assert!(is_secure_ax_element(
            "AXTextField",
            Some("AXSecureTextField")
        ));
        assert!(is_secure_ax_element("AXSecureTextField", None));
        assert!(!is_secure_ax_element("AXTextField", Some("AXSearchField")));
    }
}
