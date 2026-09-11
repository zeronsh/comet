//! Bundled native typography; browser does not load device-local font settings.
#[path = "../../../crates/ui/src/typography/bundled.rs"]
mod bundled;
struct FontFamily(gpui::SharedString);
impl gpui::Global for FontFamily {}
pub fn init(cx: &mut gpui::App) {
    let geist = bundled::register(cx, "Geist", &bundled::GEIST);
    bundled::register(cx, "Geist Mono", &bundled::GEIST_MONO);
    cx.set_global(FontFamily(
        if geist { "Geist" } else { "IBM Plex Sans" }.into(),
    ));
}
pub fn ui_rems(px: f32) -> gpui::Rems {
    gpui::rems(px / 16.0)
}
pub fn effective_family_name(cx: &gpui::App) -> gpui::SharedString {
    cx.global::<FontFamily>().0.clone()
}
pub fn generation(_: &gpui::App) -> u32 {
    0
}
