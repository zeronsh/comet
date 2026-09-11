//! Browser-page host for the web build. Remote previews start on the app origin,
//! then move to their isolated, capability-cookie-protected preview origin.
use super::model::Presentation;
use gpui::{Bounds, Pixels};
use std::{cell::Cell, rc::Rc};
use wasm_bindgen::JsCast;

#[derive(Clone)]
pub(super) struct NativePage(Rc<Inner>);

struct Inner {
    iframe: web_sys::HtmlIFrameElement,
    presentation: Cell<Presentation>,
}

impl NativePage {
    pub fn new(url: &str) -> Result<Self, String> {
        if !is_preview_url(url) {
            return Err("Only authenticated remote previews can be embedded.".into());
        }
        let document = web_sys::window()
            .and_then(|window| window.document())
            .ok_or("Browser document is unavailable")?;
        let iframe = document
            .create_element("iframe")
            .map_err(|_| "Could not create preview frame")?
            .dyn_into::<web_sys::HtmlIFrameElement>()
            .map_err(|_| "Could not create preview frame")?;
        iframe.set_title("Remote project preview");
        // Do not grant same-origin: untrusted workspace code must not read the
        // authenticated app's cookies or call its API. Every capability has its
        // own origin, so allowing that origin enables normal ES modules and fetch
        // without allowing access to the app or another preview.
        iframe
            .set_attribute(
                "sandbox",
                "allow-same-origin allow-scripts allow-forms allow-modals allow-popups allow-downloads",
            )
            .map_err(|_| "Could not isolate preview frame")?;
        iframe.set_attribute("referrerpolicy", "same-origin").ok();
        iframe.set_src(url);
        document
            .body()
            .ok_or("Browser document has no body")?
            .append_child(&iframe)
            .map_err(|_| "Could not attach preview frame")?;
        Ok(Self(Rc::new(Inner {
            iframe,
            presentation: Cell::new(Presentation::Hidden),
        })))
    }

    pub fn load(&self, url: &str) -> Result<(), String> {
        if !is_preview_url(url) {
            return Err("Only authenticated remote previews can be embedded.".into());
        }
        self.0.iframe.set_src(url);
        Ok(())
    }

    pub fn reload(&self) {
        self.0.iframe.set_src(&self.0.iframe.src());
    }

    pub fn present(&self, presentation: Presentation) {
        self.0.presentation.set(presentation);
    }

    pub fn sync(&self, bounds: Bounds<Pixels>) {
        let style = self.0.iframe.style();
        let hidden = self.0.presentation.get() == Presentation::Hidden;
        let _ = style.set_property("position", "fixed");
        let _ = style.set_property("z-index", "1");
        let _ = style.set_property("border", "0");
        let _ = style.set_property("display", if hidden { "none" } else { "block" });
        let _ = style.set_property(
            "pointer-events",
            if self.0.presentation.get() == Presentation::Live {
                "auto"
            } else {
                "none"
            },
        );
        let _ = style.set_property("left", &format!("{}px", f32::from(bounds.origin.x)));
        let _ = style.set_property("top", &format!("{}px", f32::from(bounds.origin.y)));
        let _ = style.set_property("width", &format!("{}px", f32::from(bounds.size.width)));
        let _ = style.set_property("height", &format!("{}px", f32::from(bounds.size.height)));
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.iframe.remove();
    }
}

pub(super) fn preview_url(device_id: &str, service_id: &str) -> Option<String> {
    let path = super::model::preview_route_path(device_id, service_id)?;
    let origin = web_sys::window()?.location().origin().ok()?;
    Some(format!("{origin}{path}"))
}

pub(super) fn is_preview_url(url: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Ok(origin) = window.location().origin() else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.origin().ascii_serialization() != origin {
        return false;
    }
    let mut segments = match parsed.path_segments() {
        Some(segments) => segments,
        None => return false,
    };
    let valid = matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some("api"), Some("browser"), Some("preview"), Some(device), Some(service))
            if super::model::preview_route_path(device, service).is_some()
    );
    valid
}
