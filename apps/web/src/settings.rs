//! Browser-local presentation preferences, not native persisted settings.
use gpui::{App, Global};
#[derive(Clone, Copy)]
pub enum ComposerSendBehavior {
    Enter,
    ModEnter,
}
pub fn platform_combo(key: &str) -> String {
    key.replace("mod-", "ctrl-")
}
#[derive(Clone, Default)]
pub struct BrowserSettings {
    pub code_fences_fit_content: bool,
    generation: u64,
}
impl Global for BrowserSettings {}
pub enum SavePolicy {
    Immediate,
}
pub fn current(cx: &App) -> BrowserSettings {
    cx.try_global::<BrowserSettings>()
        .cloned()
        .unwrap_or_default()
}
pub fn code_fences_generation(cx: &App) -> u64 {
    current(cx).generation
}
pub fn update(_: SavePolicy, cx: &mut App, change: impl FnOnce(&mut BrowserSettings)) {
    let mut settings = current(cx);
    let before = settings.code_fences_fit_content;
    change(&mut settings);
    if before != settings.code_fences_fit_content {
        settings.generation += 1;
    }
    cx.set_global(settings);
}
