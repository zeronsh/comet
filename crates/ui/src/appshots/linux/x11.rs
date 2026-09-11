use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use futures::channel::mpsc;
use gpui::{Image, ImageFormat as GpuiImageFormat};
use x11rb::connection::Connection;
use x11rb::image::{Image as XImage, PixelLayout};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, GrabMode, ModMask, Window};
use x11rb::rust_connection::RustConnection;

use super::atspi;
use crate::appshots::{
    AccessibilitySnapshot, CapabilityState, CaptureError, CapturedAppshot,
    validate_capture_dimensions,
};

static SHORTCUT_READY: AtomicBool = AtomicBool::new(true);

pub(super) fn shortcut_state() -> CapabilityState {
    if SHORTCUT_READY.load(Ordering::Relaxed) {
        CapabilityState::Ready
    } else {
        CapabilityState::SetupRequired
    }
}

pub(super) fn start_shortcut(tx: mpsc::UnboundedSender<()>) {
    thread::Builder::new()
        .name("appshot-x11-hotkey".into())
        .spawn(move || {
            if let Err(error) = run_shortcut(tx) {
                SHORTCUT_READY.store(false, Ordering::Relaxed);
                tracing::warn!(?error, "X11 Appshot shortcut could not be registered");
            }
        })
        .ok();
}

fn run_shortcut(tx: mpsc::UnboundedSender<()>) -> anyhow::Result<()> {
    let (connection, screen_index) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen_index].root;
    let mut previous = None;
    let mut keycode = None;
    while !tx.is_closed() {
        let shortcut = super::super::shortcut::current();
        if previous.as_ref() != Some(&shortcut) {
            if let Some(keycode) = keycode.take() {
                connection
                    .ungrab_key(keycode, root, ModMask::ANY)?
                    .check()?;
            }
            if let Some(shortcut) = &shortcut {
                match register_shortcut(&connection, root, shortcut) {
                    Ok(code) => {
                        keycode = Some(code);
                        SHORTCUT_READY.store(true, Ordering::Relaxed);
                    }
                    Err(error) => {
                        SHORTCUT_READY.store(false, Ordering::Relaxed);
                        tracing::warn!(?error, "X11 Appshot shortcut could not be registered");
                    }
                }
            }
            connection.flush()?;
            while connection.poll_for_event()?.is_some() {}
            previous = Some(shortcut.clone());
        }
        while let Some(event) = connection.poll_for_event()? {
            if let Event::KeyPress(event) = event
                && Some(event.detail) == keycode
            {
                let _ = tx.unbounded_send(());
            }
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

fn register_shortcut(
    connection: &RustConnection,
    root: Window,
    shortcut: &super::super::shortcut::Shortcut,
) -> anyhow::Result<u8> {
    let setup = connection.setup();
    let minimum = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(minimum).saturating_add(1);
    let mapping = connection.get_keyboard_mapping(minimum, count)?.reply()?;
    let per_keycode = usize::from(mapping.keysyms_per_keycode);
    anyhow::ensure!(per_keycode > 0, "X11 returned an empty keyboard map");
    let keysym =
        shortcut_keysym(&shortcut.key).ok_or_else(|| anyhow::anyhow!("unsupported Appshot key"))?;
    let keycode = mapping
        .keysyms
        .chunks(per_keycode)
        .position(|symbols| symbols.contains(&keysym))
        .map(|index| minimum.saturating_add(index as u8))
        .ok_or_else(|| anyhow::anyhow!("Appshot key is absent from the X11 keyboard map"))?;
    let base = (if shortcut.control {
        ModMask::CONTROL
    } else {
        Default::default()
    }) | (if shortcut.alt {
        ModMask::M1
    } else {
        Default::default()
    }) | (if shortcut.shift {
        ModMask::SHIFT
    } else {
        Default::default()
    }) | (if shortcut.platform {
        ModMask::M4
    } else {
        Default::default()
    });
    for modifiers in [
        base,
        base | ModMask::LOCK,
        base | ModMask::M2,
        base | ModMask::LOCK | ModMask::M2,
    ] {
        if let Err(error) = connection
            .grab_key(
                false,
                root,
                modifiers,
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()
        {
            // A partial registration must not keep consuming an old/new chord.
            connection
                .ungrab_key(keycode, root, ModMask::ANY)?
                .check()?;
            return Err(error.into());
        }
    }
    Ok(keycode)
}

fn shortcut_keysym(key: &str) -> Option<u32> {
    Some(match key {
        "space" => 0x20,
        "tab" => 0xff09,
        "enter" => 0xff0d,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "up" => 0xff52,
        "down" => 0xff54,
        "left" => 0xff51,
        "right" => 0xff53,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        key if key.len() == 1 => u32::from(key.as_bytes()[0]),
        key => 0xffbe + u32::from(key.strip_prefix('f')?.parse::<u8>().ok()?.checked_sub(1)?),
    })
}

pub(super) fn viewer_is_active() -> bool {
    let Ok((connection, screen_index)) = x11rb::connect(None) else {
        return false;
    };
    let root = connection.setup().roots[screen_index].root;
    let Ok(active_atom) = atom(&connection, b"_NET_ACTIVE_WINDOW") else {
        return false;
    };
    let Some(active) = property_u32(&connection, root, active_atom, AtomEnum::WINDOW.into())
        .and_then(|values| values.first().copied())
    else {
        return false;
    };
    window_belongs_to_viewer(&connection, active)
}

fn window_belongs_to_viewer(connection: &RustConnection, window: Window) -> bool {
    let Ok(pid_atom) = atom(connection, b"_NET_WM_PID") else {
        return false;
    };
    property_u32(connection, window, pid_atom, AtomEnum::CARDINAL.into())
        .and_then(|values| values.first().copied())
        == Some(std::process::id())
}

pub(super) async fn capture() -> Result<CapturedAppshot, CaptureError> {
    let native = capture_native()?;
    let semantic = if let Some(identity) = &native.identity {
        let semantic = atspi::capture_window(identity.pid, &identity.title)
            .await
            .ok();
        // AT-SPI and X11 have no shared window ID. A unique PID/title mapping
        // on both sides is required, and the actual XID must remain unchanged.
        if current_identity().as_ref() == Some(identity) {
            semantic
        } else {
            None
        }
    } else {
        None
    };
    let app_name = native
        .desktop
        .as_ref()
        .and_then(|desktop| desktop.name.clone())
        .or_else(|| semantic.as_ref().map(|value| value.app_name.clone()))
        .or_else(|| native.wm_class.clone())
        .unwrap_or_else(|| "X11 application".into());
    let accessibility = semantic
        .map(|value| value.snapshot)
        .unwrap_or_else(AccessibilitySnapshot::unavailable);
    let (screenshot, dimensions) = super::super::stage_appshot_png(&app_name, native.png)?;
    Ok(CapturedAppshot {
        id: uuid::Uuid::new_v4().to_string(),
        app_name,
        bundle_identifier: native
            .desktop
            .as_ref()
            .and_then(|desktop| desktop.id.clone())
            .or_else(|| native.wm_class.map(|value| format!("linux-x11:{value}"))),
        window_title: native.title,
        accessibility,
        screenshot,
        screenshot_dimensions: Some(dimensions),
        app_icon: native
            .icon_png
            .map(|bytes| Arc::new(Image::from_bytes(GpuiImageFormat::Png, bytes))),
        captured_at: chrono::Utc::now(),
    })
}

struct NativeCapture {
    identity: Option<NativeIdentity>,
    png: Vec<u8>,
    title: Option<String>,
    wm_class: Option<String>,
    desktop: Option<DesktopEntry>,
    icon_png: Option<Vec<u8>>,
}

fn capture_native() -> Result<NativeCapture, CaptureError> {
    let (connection, screen_index) = x11rb::connect(None).map_err(failed)?;
    let root = connection.setup().roots[screen_index].root;
    let active_atom = atom(&connection, b"_NET_ACTIVE_WINDOW")?;
    let active = property_u32(&connection, root, active_atom, AtomEnum::WINDOW.into())
        .and_then(|values| values.first().copied())
        .ok_or(CaptureError::NoEligibleWindow)?;
    if window_belongs_to_viewer(&connection, active) {
        return Err(CaptureError::SelfCapture);
    }
    let identity = read_identity(&connection, root, active);
    let geometry = connection
        .get_geometry(active)
        .map_err(failed)?
        .reply()
        .map_err(failed)?;
    if geometry.width == 0 || geometry.height == 0 {
        return Err(CaptureError::NoEligibleWindow);
    }
    let rgba_len =
        validate_capture_dimensions(u32::from(geometry.width), u32::from(geometry.height))?;
    let (image, visual_id) =
        XImage::get(&connection, active, 0, 0, geometry.width, geometry.height).map_err(failed)?;
    let visual = connection
        .setup()
        .roots
        .iter()
        .flat_map(|screen| &screen.allowed_depths)
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == visual_id)
        .copied()
        .ok_or_else(|| CaptureError::CaptureFailed("X11 returned an unknown visual.".into()))?;
    let layout = PixelLayout::from_visual_type(visual).map_err(failed)?;
    let mut rgba = Vec::with_capacity(rgba_len);
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            let (red, green, blue) = layout.decode(image.get_pixel(x, y));
            rgba.extend_from_slice(&[(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8, 255]);
        }
    }
    let png = encode_rgba(u32::from(geometry.width), u32::from(geometry.height), &rgba)?;
    let still_active = property_u32(&connection, root, active_atom, AtomEnum::WINDOW.into())
        .and_then(|values| values.first().copied());
    if still_active != Some(active) {
        return Err(CaptureError::CaptureFailed(
            "The active X11 window changed during capture; try again.".into(),
        ));
    }
    let title = window_title(&connection, active);
    let wm_class = window_class(&connection, active);
    let desktop = wm_class.as_deref().and_then(find_desktop_entry);
    let icon_png = property_icon(&connection, active).or_else(|| {
        desktop
            .as_ref()
            .and_then(|entry| entry.icon.as_deref())
            .and_then(load_theme_icon)
    });
    Ok(NativeCapture {
        identity,
        png,
        title,
        wm_class,
        desktop,
        icon_png,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeIdentity {
    window: Window,
    pid: u32,
    title: String,
}

fn current_identity() -> Option<NativeIdentity> {
    let (connection, screen_index) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots[screen_index].root;
    let active_atom = atom(&connection, b"_NET_ACTIVE_WINDOW").ok()?;
    let active = *property_u32(&connection, root, active_atom, AtomEnum::WINDOW.into())?.first()?;
    read_identity(&connection, root, active)
}

fn read_identity(
    connection: &RustConnection,
    root: Window,
    active: Window,
) -> Option<NativeIdentity> {
    let pid_atom = atom(connection, b"_NET_WM_PID").ok()?;
    let pid = *property_u32(connection, active, pid_atom, AtomEnum::CARDINAL.into())?.first()?;
    let title = window_title(connection, active).filter(|title| !title.trim().is_empty())?;
    let clients_atom = atom(connection, b"_NET_CLIENT_LIST").ok()?;
    let clients = property_u32(connection, root, clients_atom, AtomEnum::WINDOW.into())?;
    let mut matching = Vec::new();
    for window in clients {
        let process = property_u32(connection, window, pid_atom, AtomEnum::CARDINAL.into())
            .and_then(|values| values.first().copied());
        if process == Some(pid)
            && window_title(connection, window).as_deref() == Some(title.as_str())
        {
            matching.push(window);
        }
    }
    (pid != 0 && matching == [active]).then_some(NativeIdentity {
        window: active,
        pid,
        title,
    })
}

fn atom(connection: &RustConnection, name: &[u8]) -> Result<Atom, CaptureError> {
    connection
        .intern_atom(false, name)
        .map_err(failed)?
        .reply()
        .map(|reply| reply.atom)
        .map_err(failed)
}

fn property_u32(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    property_type: Atom,
) -> Option<Vec<u32>> {
    connection
        .get_property(false, window, property, property_type, 0, u32::MAX)
        .ok()?
        .reply()
        .ok()?
        .value32()
        .map(Iterator::collect)
}

fn property_bytes(
    connection: &RustConnection,
    window: Window,
    property: Atom,
    property_type: Atom,
) -> Option<Vec<u8>> {
    connection
        .get_property(false, window, property, property_type, 0, u32::MAX)
        .ok()?
        .reply()
        .ok()
        .map(|reply| reply.value)
}

fn window_title(connection: &RustConnection, window: Window) -> Option<String> {
    let utf8 = atom(connection, b"UTF8_STRING").ok()?;
    let net_name = atom(connection, b"_NET_WM_NAME").ok()?;
    property_bytes(connection, window, net_name, utf8)
        .or_else(|| {
            property_bytes(
                connection,
                window,
                AtomEnum::WM_NAME.into(),
                AtomEnum::STRING.into(),
            )
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|value| !value.trim().is_empty())
}

fn window_class(connection: &RustConnection, window: Window) -> Option<String> {
    let bytes = property_bytes(
        connection,
        window,
        AtomEnum::WM_CLASS.into(),
        AtomEnum::STRING.into(),
    )?;
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|value| !value.trim().is_empty())
        .next_back()
        .map(str::to_string)
}

fn property_icon(connection: &RustConnection, window: Window) -> Option<Vec<u8>> {
    let icon_atom = atom(connection, b"_NET_WM_ICON").ok()?;
    let values = property_u32(connection, window, icon_atom, AtomEnum::CARDINAL.into())?;
    let mut candidates = Vec::new();
    let mut offset = 0_usize;
    while offset + 2 <= values.len() {
        let width = values[offset] as usize;
        let height = values[offset + 1] as usize;
        offset += 2;
        let length = width.checked_mul(height)?;
        if width > 512 || height > 512 || length > 512 * 512 {
            offset = offset.checked_add(length)?;
            continue;
        }
        if width > 0 && height > 0 && offset + length <= values.len() {
            candidates.push((
                width.abs_diff(64) + height.abs_diff(64),
                width,
                height,
                &values[offset..offset + length],
            ));
        }
        offset = offset.saturating_add(length);
    }
    let (_, width, height, pixels) = candidates.into_iter().min_by_key(|value| value.0)?;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for argb in pixels {
        rgba.extend_from_slice(&[
            (argb >> 16) as u8,
            (argb >> 8) as u8,
            *argb as u8,
            (argb >> 24) as u8,
        ]);
    }
    encode_rgba(width as u32, height as u32, &rgba).ok()
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, CaptureError> {
    super::super::encode_rgba_png(width, height, rgba, "X11")
}

#[derive(Clone)]
struct DesktopEntry {
    id: Option<String>,
    name: Option<String>,
    icon: Option<String>,
}

fn find_desktop_entry(wm_class: &str) -> Option<DesktopEntry> {
    let needle = wm_class.to_ascii_lowercase();
    desktop_directories()
        .into_iter()
        .find_map(|directory| {
            fs::read_dir(directory)
                .ok()?
                .filter_map(Result::ok)
                .find_map(|item| {
                    let path = item.path();
                    if path.extension().and_then(|value| value.to_str()) != Some("desktop") {
                        return None;
                    }
                    let contents = fs::read_to_string(&path).ok()?;
                    let fields = desktop_fields(&contents);
                    let file_matches = path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case(wm_class));
                    let class_matches = fields
                        .iter()
                        .find(|(key, _)| key == "StartupWMClass")
                        .is_some_and(|(_, value)| value.eq_ignore_ascii_case(wm_class));
                    if !file_matches && !class_matches {
                        return None;
                    }
                    Some(DesktopEntry {
                        id: path
                            .file_name()
                            .and_then(|value| value.to_str())
                            .map(str::to_string),
                        name: fields
                            .iter()
                            .find(|(key, _)| key == "Name")
                            .map(|(_, value)| value.clone()),
                        icon: fields
                            .iter()
                            .find(|(key, _)| key == "Icon")
                            .map(|(_, value)| value.clone()),
                    })
                })
        })
        .or_else(|| {
            Some(DesktopEntry {
                id: Some(format!("linux-x11:{needle}")),
                name: None,
                icon: None,
            })
        })
}

fn desktop_fields(contents: &str) -> Vec<(String, String)> {
    let mut in_entry = false;
    let mut fields = Vec::new();
    for line in contents.lines() {
        if line.starts_with('[') {
            in_entry = line.trim() == "[Desktop Entry]";
            continue;
        }
        if in_entry
            && let Some((key, value)) = line.split_once('=')
            && matches!(key, "Name" | "Icon" | "StartupWMClass")
        {
            fields.push((key.into(), value.trim().into()));
        }
    }
    fields
}

fn desktop_directories() -> Vec<PathBuf> {
    let mut directories = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        directories.insert(0, PathBuf::from(home).join(".local/share/applications"));
    }
    directories
}

fn load_theme_icon(name: &str) -> Option<Vec<u8>> {
    let path = Path::new(name);
    if path.is_absolute() {
        return fs::read(path).ok();
    }
    let mut candidates = vec![
        PathBuf::from("/usr/share/pixmaps").join(format!("{name}.png")),
        PathBuf::from("/usr/share/icons/hicolor/64x64/apps").join(format!("{name}.png")),
        PathBuf::from("/usr/share/icons/hicolor/128x128/apps").join(format!("{name}.png")),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.insert(
            0,
            PathBuf::from(home).join(format!(".local/share/icons/hicolor/64x64/apps/{name}.png")),
        );
    }
    candidates.into_iter().find_map(|path| fs::read(path).ok())
}

fn failed(error: impl std::fmt::Display) -> CaptureError {
    CaptureError::CaptureFailed(format!("X11 Appshot capture failed: {error}"))
}
