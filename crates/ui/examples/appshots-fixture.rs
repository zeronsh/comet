//! Native Appshots layout evidence with isolated data. No agent messages are sent.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{path::PathBuf, sync::Arc, time::Duration};
use zeron_ui::*;
async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}
fn capture(
    window: gpui::AnyWindowHandle,
    cx: &mut AsyncApp,
    directory: &std::path::Path,
    name: &str,
) -> anyhow::Result<()> {
    window.update(cx, |_, w, cx| {
        w.draw(cx).clear();
        w.render_to_image()?
            .save(directory.join(format!("{name}.png")))?;
        Ok(())
    })?
}
fn press(window: gpui::AnyWindowHandle, cx: &mut AsyncApp, key: &str) -> anyhow::Result<()> {
    window.update(cx, |_, w, cx| {
        w.draw(cx).clear();
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        w.dispatch_event(
            gpui::PlatformInput::KeyDown(gpui::KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }),
            cx,
        );
        w.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
            cx,
        );
    })?;
    Ok(())
}
fn port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let output = PathBuf::from(std::env::args().nth(1).expect("output directory"));
    let inputs = PathBuf::from(
        std::env::args()
            .nth(2)
            .expect("fixture PNG directory: wide, tall, square"),
    );
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let core = runtime.block_on(async {
        zeron_engine::EngineCore::assemble(
            &temp.path().join("engine"),
            Arc::new(zeron_engine::default_registry()),
            zeron_proto::HarnessId::ClaudeCode,
            None,
        )
    })?;
    core.workspace
        .create_chat("appshots-fixture", None, Some(&core.device_id), None, None)?;
    core.workspace
        .rename_chat("appshots-fixture", "Review the workspace design")?;
    let ipc_port = port();
    let _ipc = runtime.block_on(zeron_engine::serve_ipc(ipc_port, core.rpc_service()))?;
    let data = temp.path().join("ui");
    std::fs::create_dir(&data)?;
    let boot = EngineBootConfig {
        data_dir: data.clone(),
        ipc_port,
        edge_url: String::new(),
        edge_token: None,
        org_id: None,
        workos_client_id: None,
        default_harness: zeron_proto::HarnessId::ClaudeCode,
    };
    let handle = runtime.block_on(state::EngineHandle::bootstrap(boot.clone()))?;
    let chats = core.workspace.read_chats()?;
    let device = core.device_id.clone();
    let mut shots = Vec::new();
    let mut paths = Vec::new();
    for (name, title, bundle) in [
        ("wide", "Fieldnotes · Product planning", "com.apple.Safari"),
        ("tall", "Design review · Notes", "com.apple.Notes"),
        ("square", "Workspace ideas", "com.apple.finder"),
    ] {
        let bytes = std::fs::read(inputs.join(format!("{name}.png")))?;
        let (screenshot, dimensions) =
            appshots::stage_appshot_png(name, bytes).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let shot = appshots::CapturedAppshot {
            id: name.into(),
            app_name: match name {
                "wide" => "Safari",
                "tall" => "Notes",
                _ => "Finder",
            }
            .into(),
            bundle_identifier: Some(bundle.into()),
            window_title: Some(title.into()),
            accessibility: appshots::AccessibilitySnapshot::unavailable(),
            screenshot,
            screenshot_dimensions: Some(dimensions),
            app_icon: None,
            captured_at: chrono::Utc::now(),
        };
        let path = inputs
            .join(format!("{name}.png"))
            .to_string_lossy()
            .into_owned();
        attachments::store_loaded(
            &device,
            &path,
            shot.screenshot.name.clone().into(),
            shot.screenshot.image.clone(),
        );
        paths.push(path);
        shots.push(shot);
    }
    let body = appshots::with_appshots(
        "Compare these layouts and suggest a clearer hierarchy.",
        &shots,
        &shots
            .iter()
            .zip(&paths)
            .map(|(s, p)| (s.screenshot.id.clone(), p.clone()))
            .collect(),
    );
    let message = attachments::with_attachments(&body, &paths);
    let queue = vec![
        zeron_doc::QueuedMessage {
            id: "queued-review".into(),
            text: body,
            attachments: paths.clone(),
            hold_for_turn_end: true,
            issued_by: device.clone(),
            issued_at: 1788900000000,
            edited_at: None,
            delivery_gate: None,
        },
        zeron_doc::QueuedMessage {
            id: "queued-text".into(),
            text: "Then check the spacing and keyboard navigation.".into(),
            attachments: vec![],
            hold_for_turn_end: true,
            issued_by: device.clone(),
            issued_at: 1788900000001,
            edited_at: None,
            delivery_gate: None,
        },
    ];
    let failure = Arc::new(std::sync::Mutex::new(None));
    let result = failure.clone();
    gpui_platform::application().with_assets(icons::Assets).run(move |cx| {
        gpui_tokio::init(cx); gpui_base::init(cx);
        let settings=settings::UiSettings::default(); settings::init(settings.clone(),data.clone(),cx);
        let fonts=typography::register_fonts(cx); typography::init(settings.ui_font_family.clone(),settings.ui_font_size,fonts,cx);
        theme_library::init(data.clone(),cx); appearance::init(appearance::AppearanceMode::Dark,settings.theme_selection,settings.accent,settings.surface,cx);
        history::init(settings.git_history_columns,settings.git_history_column_widths,settings.git_history_column_order,settings.git_history_author_display,cx);
        composer::init(cx,settings.composer_send_behavior); terminal::panel::init(cx); app_menus::init(cx);
        let state=cx.new(|_| { let mut s=state::AppState::new(); s.fixture_attachment_engine(handle); s.connection=zeron_proto::view::ConnectionStatus::Ready; s.workspace_scope=Some(zeron_proto::WorkspaceScope::Development); s.local_device_id=Some(device.clone()); s.devices=vec![serde_json::from_value(serde_json::json!({"id":device,"name":"This device","platform":std::env::consts::OS,"lastSeenAt":null})).unwrap()]; s.chats=chats; s.selected_chat=Some("appshots-fixture".into()); s.auto_selected=true; s.chats_synced=true; s.spaces_synced=true; s });
        let window=cx.open_window(WindowOptions {window_background:theme::Theme::of(cx).window_background_appearance(),window_bounds:Some(WindowBounds::Windowed(Bounds::new(gpui::point(px(20.),px(40.)),size(px(1100.),px(850.))))),..Default::default()},|_,cx|cx.new(|cx|shell::Shell::new(state.clone(),boot,cx))).unwrap();
        state.update(cx,|_,cx|cx.notify()); cx.activate(true);
        cx.spawn(async move |cx| {
            let run:anyhow::Result<()>=async {
                pause(cx,1000).await;
                state.update(cx,|s,cx| {s.receive_transcript_frame(zeron_doc::TranscriptFrame::Reset {reset:serde_json::from_value(serde_json::json!([
                    {"id":"user","role":"user","parts":[{"id":"text","kind":"text","text":message}],"createdAt":1788900000000_i64,"deviceId":device},
                    {"id":"assistant","role":"assistant","parts":[{"id":"text","kind":"text","text":"The three Appshots show a consistent visual style. I’ll compare the spacing and reading order, then check how the layout adapts to smaller screens."}],"createdAt":1788900001000_i64,"deviceId":device,"status":"complete"}
                ])).unwrap()},cx).unwrap();cx.notify();});
                pause(cx,900).await;
                window.update(cx,|s,_,cx|s.fixture_appshots_transcript_start(cx))?;
                pause(cx,400).await;capture(window.into(),cx,&output,"appshots-transcript-dark-1100")?;
                state.update(cx,|s,cx|{s.queue=queue;cx.notify();});
                let evidence_shots=shots.clone();
                let composer=window.update(cx,|s,_,_|s.fixture_appshots_composer())?;
                composer.update(cx,|c,cx| {for mut shot in shots {
                    shot.app_icon=appshots::presentation_icon(&appshots::AppshotPresentation {app_name:shot.app_name.clone(),window_title:shot.window_title.clone(),bundle_identifier:shot.bundle_identifier.clone()});
                    c.stage_appshot(shot,cx);
                } cx.notify();});
                pause(cx,1800).await; capture(window.into(), cx, &output,"appshots-chat-dark-1100")?;
                cx.update(|cx|appearance::set_mode(appearance::AppearanceMode::Light,cx));pause(cx,500).await;capture(window.into(), cx, &output,"appshots-chat-light-1100")?;
                for width in [700.,390.,320.] {
                    window.update(cx,|s,w,cx|{ s.fixture_appshots_sidebar(true,cx); w.resize(size(px(width),px(850.))); })?;
                    pause(cx,700).await;capture(window.into(), cx, &output,&format!("appshots-chat-light-{width}"))?;
                }
                window.update(cx,|s,w,cx| {s.fixture_appshots_sidebar(false,cx);w.resize(size(px(1100.),px(850.)));s.fixture_appshots_settings(true,cx);})?;
                pause(cx,900).await;capture(window.into(), cx, &output,"appshots-settings-light-1100")?;
                cx.update(|cx|appearance::set_mode(appearance::AppearanceMode::Dark,cx));pause(cx,500).await;capture(window.into(), cx, &output,"appshots-settings-dark-1100")?;
                press(window.into(),cx,"tab")?;press(window.into(),cx,"space")?;
                pause(cx,500).await;capture(window.into(),cx,&output,"appshots-settings-enabled-keyboard")?;
                for key in ["tab","tab","tab","tab","space"] { press(window.into(),cx,key)?; }
                pause(cx,500).await;capture(window.into(),cx,&output,"appshots-settings-last-session")?;
                for key in ["shift-tab","shift-tab","enter"] { press(window.into(),cx,key)?; }
                pause(cx,500).await;capture(window.into(),cx,&output,"appshots-settings-recording")?;
                press(window.into(),cx,"escape")?;
                window.update(cx,|_,w,_|w.resize(size(px(600.),px(850.))))?;pause(cx,700).await;capture(window.into(), cx, &output,"appshots-settings-dark-600")?;
                window.update(cx,|s,w,cx| {w.resize(size(px(1100.),px(850.)));s.fixture_appshots_settings(false,cx);})?;
                composer.update(cx,|c,cx|c.fixture_clear_appshots(cx));
                let state_paths=vec![paths[0].clone(), "pending://appshot-test/notes.png".into(), "/missing/appshot-test.png".into()];
                attachments::store_error(&device,&state_paths[2]);
                let state_body=appshots::with_appshots("Appshots keep their source labels while images are loading or unavailable.",&evidence_shots,&evidence_shots.iter().zip(&state_paths).map(|(s,p)|(s.screenshot.id.clone(),p.clone())).collect());
                let state_message=attachments::with_attachments(&state_body,&state_paths);
                state.update(cx,|s,cx|{s.queue.clear();s.receive_transcript_frame(zeron_doc::TranscriptFrame::Reset{reset:serde_json::from_value(serde_json::json!([
                    {"id":"states-user","role":"user","parts":[{"id":"text","kind":"text","text":state_message}],"createdAt":1788900000000_i64,"deviceId":device}
                ])).unwrap()},cx).unwrap();cx.notify();});
                pause(cx,700).await;capture(window.into(),cx,&output,"appshots-transcript-transfer-states")?;
                window.update(cx,|s,w,cx|{s.fixture_appshots_sidebar(true,cx);w.resize(size(px(390.),px(850.)));s.fixture_appshots_transcript_start(cx);})?;
                pause(cx,600).await;capture(window.into(),cx,&output,"appshots-transcript-narrow-390")?;
                std::fs::write(output.join("result.txt"),"Rendered production Shell/Composer/Transcript/Settings with neutral fixture captures. Narrow desktop widths are not physical iOS validation. No native capture or remote transport claims.\n")?;
                Ok(())
            }.await;
            if let Err(error)=run {eprintln!("Appshots fixture failed: {error:#}");*result.lock().unwrap()=Some(format!("{error:#}"));}
            let _=window.update(cx,|_,w,_|w.remove_window());pause(cx,100).await;cx.update(|cx|cx.quit());
        }).detach();
    });
    runtime.block_on(core.shutdown());
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
