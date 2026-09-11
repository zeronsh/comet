//! Browser entry point for the production shared `zeron_ui::Shell`.
//!
//! The browser owns only its cookie-authenticated gateway session and one remote
//! DeviceRoom transport. It never starts, probes, or embeds an engine.

#[cfg(target_arch = "wasm32")]
mod browser_session;
#[cfg(target_arch = "wasm32")]
mod rpc;

#[cfg(target_arch = "wasm32")]
use gpui::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use zeron_proto::HarnessId;
#[cfg(target_arch = "wasm32")]
use zeron_ui::state::AppState;
#[cfg(target_arch = "wasm32")]
use zeron_ui::{
    EngineBootConfig, app_menus, appearance, composer, history, settings, shell, terminal,
    theme_library, typography,
};

#[cfg(target_arch = "wasm32")]
fn main() {
    gpui_platform::web_init();
    let app = gpui_platform::application()
        .with_assets(zeron_ui::icons::Assets)
        .run_embedded(|cx: &mut App| {
            let data_dir = settings::browser_preferences_namespace();
            let ui_settings = settings::UiSettings::load(&data_dir);
            settings::init(ui_settings.clone(), data_dir.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(
                ui_settings.ui_font_family.clone(),
                ui_settings.ui_font_size,
                fonts,
                cx,
            );
            theme_library::init(data_dir.clone(), cx);
            appearance::init(
                ui_settings.appearance,
                ui_settings.theme_selection,
                ui_settings.accent,
                ui_settings.surface,
                cx,
            );
            history::init(
                ui_settings.git_history_columns,
                ui_settings.git_history_column_widths,
                ui_settings.git_history_column_order,
                ui_settings.git_history_author_display,
                cx,
            );
            composer::init(cx, ui_settings.composer_send_behavior);
            terminal::panel::init(cx);
            app_menus::init(cx);

            let boot = EngineBootConfig {
                data_dir,
                // Browser clients always attach to a selected remote DeviceRoom.
                ipc_port: 0,
                edge_url: "browser://same-origin".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: HarnessId::ClaudeCode,
            };

            let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |window, cx| {
                    let state = cx.new(|_| AppState::new());
                    let shell = cx.new(|cx| shell::Shell::new(state.clone(), boot.clone(), cx));
                    let browser_session = browser_session::BrowserSession::start(
                        state,
                        boot.clone(),
                        window.window_handle(),
                        cx,
                    );
                    shell::set_external_lifecycle_handler(Some(Rc::new(move |action| {
                        browser_session.handle_external_lifecycle_action(action);
                    })));
                    shell
                },
            )
            .expect("browser shell should open its GPUI window");
            cx.activate(true);
        });
    // The embedded application must live for the lifetime of the page.
    std::mem::forget(app);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
