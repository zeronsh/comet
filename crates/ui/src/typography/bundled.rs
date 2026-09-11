//! Bundled interface faces shared by the native and browser text systems.
use gpui::App;
use std::borrow::Cow;

pub const GEIST: [&[u8]; 8] = [
    include_bytes!("../../assets/fonts/Geist.ttf"),
    include_bytes!("../../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../../assets/fonts/Geist-MediumItalic.ttf"),
    include_bytes!("../../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/Geist-SemiBoldItalic.ttf"),
    include_bytes!("../../assets/fonts/Geist-Bold.ttf"),
    include_bytes!("../../assets/fonts/Geist-BoldItalic.ttf"),
];
pub const GEIST_MONO: [&[u8]; 8] = [
    include_bytes!("../../assets/fonts/GeistMono.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-Italic.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-MediumItalic.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-SemiBoldItalic.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-Bold.ttf"),
    include_bytes!("../../assets/fonts/GeistMono-BoldItalic.ttf"),
];
pub fn register(cx: &App, label: &str, faces: &'static [&'static [u8]]) -> bool {
    let fonts = faces.iter().map(|face| Cow::Borrowed(*face)).collect();
    match cx.text_system().add_fonts(fonts) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(font_family = label, error = %err, "failed to register bundled font family");
            false
        }
    }
}
