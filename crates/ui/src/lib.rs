//! zeron-ui — the gpui viewport. Shell, sidebar, conversation, composer, terminal,
//! diff pane.
//!
//! Design: ARCHITECTURE.md §4; animation catalog docs/research/feature-inventory.md
//! §1.12; virtualization/markdown techniques docs/research/mugen-pretext.md.
//!
//! M3a foundation:
//! - [`theme`] — always-dark monochrome theme (oklch-derived neutrals), a gpui Global;
//! - [`motion`] — the zeron animation catalog over gpui `Animation` + cubic-bezier;
//! - [`state`] — `AppState` entity + `EngineHandle` (connect-or-embed engine);
//! - [`settings`] — persisted pane widths/collapse flags;
//! - [`shell`] — sidebar + main panel + right-pane scaffold + gate;
//! - [`loaders`] — zeron pulse loader, gradient spinner, boot splash.

pub mod app_menus;
pub mod appearance;
pub mod appshots;
pub mod attachments;
pub mod badges;
pub mod browser;
pub mod change_requests;
pub mod changes;
mod comment_ui;
pub mod comments;
pub mod composer;
mod context_usage;
pub mod edge_fade;
pub mod files;
pub mod frost;
pub mod history;
pub mod icons;
pub mod links;
pub mod loaders;
pub mod markdown;
pub mod motion;
pub mod notify;
pub mod pickers;
pub mod popover;
pub mod queue;
pub mod rail;
pub mod settings;
pub mod shell;
pub mod sound;
pub mod state;
pub(crate) mod surface_chrome;
pub mod syntax_cache;
pub mod terminal;
pub mod theme;
pub mod theme_library;
pub mod transcript;
pub mod typography;
mod workspace_links;

use std::path::PathBuf;

use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

pub use state::EngineBootConfig;
pub use zeron_proto::HarnessId;

/// Everything the headed binary passes in (config/env resolution lives in
/// `apps/zeron`, not here).
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// Data directory — engine stores + `ui-settings.json`.
    pub data_dir: PathBuf,
    /// Localhost IPC port: connect if an engine daemon is listening, embed if not.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Edge bearer; `None` runs offline.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// WorkOS client id; `Some` makes the embedded headed engine require a
    /// production session before opening identity-scoped stores.
    pub workos_client_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
    /// Conversation URL passed by the OS on a cold launch.
    pub initial_url: Option<String>,
}

impl UiConfig {
    fn boot(&self) -> EngineBootConfig {
        EngineBootConfig {
            data_dir: self.data_dir.clone(),
            ipc_port: self.ipc_port,
            edge_url: self.edge_url.clone(),
            edge_token: self.edge_token.clone(),
            org_id: self.org_id.clone(),
            workos_client_id: self.workos_client_id.clone(),
            default_harness: self.default_harness,
        }
    }
}

/// What a dock-icon reopen needs to rebuild the main window after ⌘W closed it
/// (macOS keeps the process alive with just the menu bar, like zed).
struct ReopenState {
    state: gpui::Entity<state::AppState>,
    boot: EngineBootConfig,
}

impl gpui::Global for ReopenState {}

/// Run the headed app: tokio bridge up, engine bootstrap kicked off (probe →
/// connect-or-embed), 1320×880 window (min 900×600) with [`shell::Shell`] as the
/// root view, boot splash overlaid until the engine reports ready.
pub fn run_app(config: UiConfig) {
    let app = gpui_platform::application().with_assets(icons::Assets);
    let (url_tx, mut url_rx) = futures::channel::mpsc::unbounded::<String>();
    let callback_tx = url_tx.clone();
    app.on_open_urls(move |urls| {
        for url in urls {
            let _ = callback_tx.unbounded_send(url);
        }
    });
    if let Some(url) = config.initial_url.clone() {
        let _ = url_tx.unbounded_send(url);
    }
    // Dock-icon click with no window (⌘W closed it): rebuild the main window
    // around the still-running engine — zed does the same via `on_reopen`
    // (crates/zed/src/main.rs `app.on_reopen`).
    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<ReopenState>()
        {
            let (state, boot) = (reopen.state.clone(), reopen.boot.clone());
            open_main_window(state, boot, cx);
        }
    });
    app.run(move |cx: &mut App| {
        // NB: pinned-rev API — `gpui_tokio::init(cx)` free function (not `Tokio::init`).
        gpui_tokio::init(cx);
        gpui_base::init(cx);
        let data_dir = config.boot().data_dir.clone();
        let ui_settings = settings::UiSettings::load(&data_dir);
        settings::init(ui_settings.clone(), data_dir.clone(), cx);
        let font_availability = typography::register_fonts(cx);
        // Typography first: theme installation reads the effective family, so
        // the first frame has the final font and palette without a flash.
        typography::init(
            ui_settings.ui_font_family.clone(),
            ui_settings.ui_font_size,
            font_availability,
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
        appshots::set_enabled(ui_settings.appshots_enabled);
        terminal::panel::init(cx);
        app_menus::init(cx);
        cx.register_url_scheme("zeron").detach();

        let state = cx.new(|_| state::AppState::new());
        let url_state = state.clone();
        cx.spawn(async move |cx| {
            while let Some(url) = url_rx.next().await {
                url_state.update(cx, |state, cx| state.open_deep_link(&url, cx));
            }
        })
        .detach();
        state::AppState::bootstrap(state.clone(), config.boot(), cx);

        // Graceful teardown: an in-process engine drains live runs and flushes
        // doc snapshots before the process exits (remote engines outlive us).
        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            settings::flush(cx);
            let shutdown =
                quit_state.read(cx).engine().cloned().map(|handle| {
                    gpui_tokio::Tokio::spawn(cx, async move { handle.shutdown().await })
                });
            async move {
                if let Some(task) = shutdown {
                    let _ = task.await;
                }
            }
        })
        .detach();

        cx.set_global(ReopenState {
            state: state.clone(),
            boot: config.boot(),
        });
        open_main_window(state, config.boot(), cx);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        start_appshot_service(config.boot().data_dir, cx);
        // Native menu bar — macOS gets the standard app menu (About/Services/
        // Hide/Quit ⌘Q), Edit clipboard verbs routed to the focused input, and
        // a Window menu (⌘M/⌘W). Without this, `NSApp.mainMenu` stays nil: no
        // Cmd+Q, and nothing for the system menu bar to show. Set after
        // `open_main_window` because `Shell::new` ran `apply_keymap`
        // synchronously, so `set_menus` reads the final bindings for the ⌘-key
        // equivalents (gpui snapshots the keymap at set time).
        cx.set_menus(app_menus::app_menus());
        cx.activate(true);
    });
}

/// Open the 1320×880 main window (min 900×600) with [`shell::Shell`] as the
/// root view. Called at boot and again from `on_reopen` if the dock icon is
/// clicked after ⌘W closed the window.
fn open_main_window(
    state: gpui::Entity<state::AppState>,
    boot: EngineBootConfig,
    cx: &mut App,
) -> gpui::WindowHandle<shell::Shell> {
    // zeron window geometry: 1320×880, min 900×600 (feature-inventory §1.1).
    let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(600.))),
                // `kind` is deliberately left at its default `WindowKind::Normal`
                // (gpui platform.rs WindowOptions::default), which on macOS maps
                // to `NSNormalWindowLevel` (gpui_macos window.rs) — same as zed's
                // main window. Nothing here raises the window level or touches
                // presentation options; the "menu bar never appears" symptom came
                // from the missing `set_menus` call (nil `NSApp.mainMenu`), not
                // from window kind/level, and `appears_transparent` only affects
                // the titlebar, not the menu bar.
                // macOS: frameless-inset chrome like the original Electron app
                // (`titleBarStyle: "hiddenInset"`, traffic lights at 14,15 —
                // feature-inventory §1.1). No title text — the strip is
                // custom-drawn (zed sets `title: None` the same way). On
                // Linux/Windows `appears_transparent` hides the system titlebar
                // for our custom-drawn chrome; harmless where unsupported.
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    // Centered on the titlebar's content line (40px bar, content
                    // shifted 4px down, lights ~12px tall → center 22).
                    traffic_light_position: Some(gpui::point(px(14.), px(14.))),
                }),
                // Our own titlebar strip drags the window (WindowControlArea::
                // Drag + start_window_move) — mark the content view app-owned
                // so AppKit neither dead-zones the strip nor delays clicks.
                app_owns_titlebar_drag: true,
                // Linux: request client-side decorations — zeron draws its own
                // unified titlebar and (under CSD) its own caption buttons
                // (shell.rs `render_linux_caption_controls`). Leaving this unset
                // requests SERVER decorations, which stacked a compositor
                // titlebar on top of the app's chrome under sway/KDE, while
                // compositors without SSD support (GNOME) went client-side
                // anyway — frameless, and before the shell drew caption buttons,
                // with no window controls at all. The compositor can still
                // override via xdg-decoration negotiation; the shell re-resolves
                // what to draw every frame.
                window_decorations: cfg!(target_os = "linux")
                    .then_some(gpui::WindowDecorations::Client),
                // Frosted shell (macOS): blur the desktop behind the window; the
                // shell paints its frost surface translucent so the sidebar reads
                // as glass (shell.rs root). Elsewhere blur support is compositor
                // roulette — stay opaque.
                // One source of truth with the re-apply loop in `appearance::apply`
                // — if these two ever disagree, vibrancy dies on the first theme
                // change and never comes back.
                window_background: theme::Theme::of(cx).window_background_appearance(),
                app_id: Some("zeron".into()),
                ..Default::default()
            },
            move |window, cx| {
                window.set_rem_size(px(typography::font_size(cx).pixels()));
                // React to the user flipping macOS between light and dark. Detached:
                // the subscription lives as long as the window does, and the window
                // owns nothing that would drop it early.
                appearance::observe_window(window, cx).detach();
                let shell = cx.new(|cx| shell::Shell::new(state, boot, cx));
                let weak_shell = shell.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    weak_shell
                        .update(cx, |shell, cx| shell.prepare_window_close(cx))
                        .unwrap_or(true)
                });
                shell
            },
        )
        .expect("failed to open window");
    // Belt and braces: assert the blur once the window actually exists. The
    // `WindowOptions` value is applied during creation, before the view is
    // attached; re-pushing it here means a window is never left opaque.
    appearance::reapply_window_background(cx);
    handle
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn start_appshot_service(activation_dir: std::path::PathBuf, cx: &mut App) {
    let mut shortcuts = appshots::start_global_shortcut(activation_dir);
    cx.spawn(async move |cx| {
        while shortcuts.next().await.is_some() {
            if !appshots::capture_allowed() {
                continue;
            }
            let Some(capture) = cx.update(start_appshot_capture) else {
                continue;
            };
            let capture = capture.await;
            // Coalesce presses made while capture was in flight. Delivery
            // focuses Zeron; replaying old activations would capture the wrong
            // app or show a misleading self-capture error after success.
            while matches!(shortcuts.next().now_or_never(), Some(Some(()))) {}
            cx.update(|cx| deliver_appshot(capture, cx));
        }
    })
    .detach();
}

/// Check viewer focus on the UI thread before any native capture or portal
/// request. Portals do not identify the source window, so their backends cannot
/// reject Zeron after the picker or capture has already started.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn start_appshot_capture(
    cx: &mut App,
) -> Option<gpui::Task<Result<appshots::CapturedAppshot, appshots::CaptureError>>> {
    if cx.active_window().is_some() {
        return None;
    }
    Some(
        cx.background_executor()
            .spawn(async { appshots::capture_active_window().await }),
    )
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod appshot_activation_tests {
    use super::*;

    struct ViewerWindow;

    impl gpui::Render for ViewerWindow {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    #[gpui::test]
    fn appshot_capture_skips_any_focused_viewer_window(cx: &mut gpui::TestAppContext) {
        // The guard must cover every Zeron window, not only a Shell/chat root.
        for _ in 0..2 {
            let window = cx.add_window(|_, _| ViewerWindow);
            window
                .update(cx, |_, window, _| window.activate_window())
                .unwrap();
            cx.run_until_parked();
            cx.update(|cx| {
                assert!(cx.active_window().is_some());
                assert!(start_appshot_capture(cx).is_none());
            });
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn deliver_appshot(
    result: Result<appshots::CapturedAppshot, appshots::CaptureError>,
    cx: &mut App,
) {
    use std::collections::VecDeque;
    use std::sync::{Mutex, OnceLock};

    fn pending() -> &'static Mutex<VecDeque<appshots::CapturedAppshot>> {
        static PENDING: OnceLock<Mutex<VecDeque<appshots::CapturedAppshot>>> = OnceLock::new();
        PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
    }

    if matches!(
        result,
        Err(appshots::CaptureError::Cancelled | appshots::CaptureError::SelfCapture)
    ) {
        return;
    }
    let mut captures = pending()
        .lock()
        .map(|mut queue| queue.drain(..).collect::<VecDeque<_>>())
        .unwrap_or_default();
    let error = match result {
        Ok(appshot) => {
            captures.push_back(appshot);
            None
        }
        Err(error) => Some(error),
    };
    let handle = cx
        .window_stack()
        .unwrap_or_else(|| cx.windows())
        .into_iter()
        .find_map(|handle| handle.downcast::<shell::Shell>())
        .or_else(|| {
            let reopen = cx.try_global::<ReopenState>()?;
            Some(open_main_window(
                reopen.state.clone(),
                reopen.boot.clone(),
                cx,
            ))
        });
    let Some(handle) = handle else {
        if !captures.is_empty() {
            let count = captures.len();
            if let Ok(mut queue) = pending().lock() {
                queue.extend(captures);
            }
            tracing::warn!(
                count,
                "Appshot captured with no Zeron window; preserving it for the next delivery"
            );
        }
        return;
    };
    let captured = !captures.is_empty();
    cx.activate(true);
    let _ = handle.update(cx, |shell, window, cx| {
        window.activate_window();
        for appshot in captures {
            shell.receive_appshot(appshot, window, cx);
        }
        if let Some(error) = error {
            shell.show_appshot_error(error.to_string(), window, cx);
        }
    });
    if captured {
        appshots::foreground_after_capture();
    }
}
