//! Shared user preference and live registration updates for desktop hotkeys.
use std::sync::{Mutex, OnceLock};

use futures::channel::mpsc;
use gpui::Keystroke;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shortcut {
    pub key: String,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

impl Shortcut {
    pub fn parse(combo: &str) -> Result<Self, ()> {
        let stroke = Keystroke::parse(&crate::settings::platform_combo(combo)).map_err(|_| ())?;
        let modifiers = stroke.modifiers;
        let key = stroke.key;
        let supported = (key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric())
            || matches!(
                key.as_str(),
                "space"
                    | "tab"
                    | "enter"
                    | "backspace"
                    | "delete"
                    | "insert"
                    | "up"
                    | "down"
                    | "left"
                    | "right"
                    | "home"
                    | "end"
                    | "pageup"
                    | "pagedown"
            )
            || key
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| (1..=20).contains(&n));
        if !supported
            || modifiers.function
            || !(modifiers.control || modifiers.alt || modifiers.platform)
        {
            return Err(());
        }
        Ok(Self {
            key,
            control: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            platform: modifiers.platform,
        })
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn portal_trigger(&self) -> String {
        let mut parts = Vec::new();
        if self.control {
            parts.push("CTRL".to_owned());
        }
        if self.alt {
            parts.push("ALT".to_owned());
        }
        if self.shift {
            parts.push("SHIFT".to_owned());
        }
        if self.platform {
            parts.push("LOGO".to_owned());
        }
        parts.push(match self.key.as_str() {
            "enter" => "Return".into(),
            "backspace" => "BackSpace".into(),
            "pageup" => "Page_Up".into(),
            "pagedown" => "Page_Down".into(),
            "space" => "space".into(),
            key if key.len() == 1 => key.into(),
            key => {
                let mut chars = key.chars();
                chars.next().unwrap().to_uppercase().collect::<String>() + chars.as_str()
            }
        });
        parts.join("+")
    }
}

struct Preferences {
    shortcut: Shortcut,
    recording: bool,
    subscribers: Vec<mpsc::UnboundedSender<Option<Shortcut>>>,
}

fn preferences() -> &'static Mutex<Preferences> {
    static PREFERENCES: OnceLock<Mutex<Preferences>> = OnceLock::new();
    PREFERENCES.get_or_init(|| {
        Mutex::new(Preferences {
            shortcut: Shortcut::parse(crate::settings::ShortcutId::CaptureAppshot.default_combo())
                .unwrap(),
            recording: false,
            subscribers: Vec::new(),
        })
    })
}

impl Preferences {
    fn active(&self) -> Option<Shortcut> {
        (super::enabled() && !self.recording).then(|| self.shortcut.clone())
    }
}

pub(crate) fn current() -> Option<Shortcut> {
    preferences().lock().unwrap().active()
}

#[cfg(target_os = "linux")]
pub(crate) fn subscribe() -> mpsc::UnboundedReceiver<Option<Shortcut>> {
    let (tx, rx) = mpsc::unbounded();
    let mut preferences = preferences().lock().unwrap();
    let _ = tx.unbounded_send(preferences.active());
    preferences.subscribers.push(tx);
    rx
}

fn update(mutate: impl FnOnce(&mut Preferences)) {
    let active = {
        let mut preferences = preferences().lock().unwrap();
        mutate(&mut preferences);
        let active = preferences.active();
        preferences
            .subscribers
            .retain(|tx| tx.unbounded_send(active.clone()).is_ok());
        active
    };
    #[cfg(target_os = "macos")]
    super::macos::refresh_global_shortcut(active);
    #[cfg(not(target_os = "macos"))]
    let _ = active;
}

pub(super) fn enabled_changed() {
    update(|_| {});
}

pub fn set_shortcut(combo: &str) {
    if !super::is_desktop() {
        return;
    }
    let shortcut = Shortcut::parse(combo).unwrap_or_else(|_| {
        Shortcut::parse(crate::settings::ShortcutId::CaptureAppshot.default_combo()).unwrap()
    });
    if preferences().lock().unwrap().shortcut == shortcut {
        return;
    }
    update(|preferences| preferences.shortcut = shortcut);
}

pub fn set_recording(recording: bool) {
    if !super::is_desktop() || preferences().lock().unwrap().recording == recording {
        return;
    }
    update(|preferences| preferences.recording = recording);
}

pub fn validate_shortcut(combo: &str) -> Result<(), ()> {
    Shortcut::parse(combo).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_shortcuts_require_a_supported_modified_key() {
        for combo in ["a", "shift-a", "fn-a", "ctrl-mystery", "ctrl-f25"] {
            assert!(Shortcut::parse(combo).is_err(), "{combo}");
        }
        for combo in ["ctrl-alt-space", "mod-shift-k", "alt-f12", "ctrl-pageup"] {
            assert!(Shortcut::parse(combo).is_ok(), "{combo}");
        }
    }
    #[test]
    fn portal_uses_xkb_key_names_and_selected_modifiers() {
        assert_eq!(
            Shortcut::parse("ctrl-alt-space").unwrap().portal_trigger(),
            "CTRL+ALT+space"
        );
        assert_eq!(
            Shortcut::parse("ctrl-shift-pageup")
                .unwrap()
                .portal_trigger(),
            "CTRL+SHIFT+Page_Up"
        );
    }
}

pub(crate) fn capture_allowed() -> bool {
    current().is_some()
}
