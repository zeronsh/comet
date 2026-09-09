//! Real project process discovery → RPC → native browser → stable proxy / HMR.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{path::PathBuf, sync::Arc, time::Duration};
use zeron_ui::*;
async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}
fn capture(directory: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let path = directory.join(format!("{name}.png"));
    #[cfg(target_os = "macos")]
    let status = {
        let app = objc2_app_kit::NSApplication::sharedApplication(
            objc2::MainThreadMarker::new().unwrap(),
        );
        let window = app
            .keyWindow()
            .or_else(|| app.mainWindow())
            .ok_or_else(|| anyhow::anyhow!("fixture window is not available"))?;
        std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-l", &window.windowNumber().to_string()])
            .arg(&path)
            .status()?
    };
    #[cfg(not(target_os = "macos"))]
    let status = {
        let capture_window = std::env::var("ZERON_BROWSER_CAPTURE_WINDOW").ok();
        let windows = std::process::Command::new("xdotool")
            .args([
                "search",
                "--onlyvisible",
                "--pid",
                &std::process::id().to_string(),
            ])
            .output()?;
        let id = capture_window
            .or_else(|| {
                String::from_utf8(windows.stdout)
                    .ok()?
                    .lines()
                    .next()
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow::anyhow!("fixture window not visible"))?;
        std::process::Command::new("import")
            .args(["-window", &id])
            .arg(&path)
            .status()?
    };
    anyhow::ensure!(status.success(), "screenshot capture failed");
    Ok(())
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
    let output = PathBuf::from(std::env::args().nth(1).expect("capture directory"));
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("fieldnotes");
    std::fs::create_dir(&project)?;
    let html = "<!doctype html><meta charset=\"utf-8\"><title>Fieldnotes</title><style>body{background:#f6f5ed;color:#263d31;font:15px system-ui;padding:32px}h1{font:44px Georgia;letter-spacing:-2px}p{line-height:1.7;color:#5d6d63}small{letter-spacing:2px}article{border:1px solid #d6ded3;border-radius:12px;padding:22px;margin:30px 0}a{color:#345d48}</style><small>FIELDNOTES / YOUR WORKSPACE</small><h1>Room for your next idea.</h1><p>A quiet place to collect thoughts, explore possibilities, and bring your work to life.</p><article><b>Make something thoughtful.</b><p>Your project is live beside your conversation. Changes appear as you work.</p></article><a href='/'>Explore your workspace →</a>";
    std::fs::write(project.join("index.html"), html)?;
    std::fs::write(
        project.join("api.js"),
        "require('http').createServer((q,s)=>{s.setHeader('Content-Type','application/json');s.end(JSON.stringify({status:'ok'}))}).listen(Number(process.argv[2]),'127.0.0.1')",
    )?;
    let vite = std::env::var("VITE_BINARY").expect("VITE_BINARY is the installed vite/bin/vite.js");
    let start = |port: u16| -> anyhow::Result<Child> {
        Ok(Child(
            std::process::Command::new("node")
                .arg(&vite)
                .args([
                    "--host",
                    "127.0.0.1",
                    "--strictPort",
                    "--port",
                    &port.to_string(),
                ])
                .current_dir(&project)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()?,
        ))
    };
    let first_port = port();
    let vite_child = start(first_port)?;
    let api = Child(
        std::process::Command::new("node")
            .arg("api.js")
            .arg(port().to_string())
            .current_dir(&project)
            .spawn()?,
    );
    let runtime = tokio::runtime::Runtime::new()?;
    let core = runtime.block_on(async {
        zeron_engine::EngineCore::assemble(
            &temp.path().join("engine"),
            Arc::new(zeron_engine::default_registry()),
            zeron_proto::HarnessId::ClaudeCode,
            None,
        )
    })?;
    core.workspace.create_space(
        "project",
        &core.device_id,
        &project.to_string_lossy(),
        Some("Fieldnotes".into()),
        false,
    )?;
    core.workspace.create_chat(
        "preview-fixture",
        Some("project"),
        None,
        None,
        Some(project.to_string_lossy().into_owned()),
    )?;
    core.workspace
        .rename_chat("preview-fixture", "Build the Fieldnotes workspace")?;
    let root = project.clone();
    runtime.block_on(
        core.previews
            .start(Arc::new(move || vec![root.clone()]), None),
    );
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
    let spaces = core.workspace.read_spaces()?;
    let device = core.device_id.clone();
    let failure = Arc::new(std::sync::Mutex::new(None));
    let result = failure.clone();
    let second_port = port();
    let vite_restart = vite.clone();
    let restart_root = project.clone();
    gpui_platform::application().with_assets(icons::Assets).run(move |cx| {
        gpui_tokio::init(cx); gpui_base::init(cx);
        let settings = settings::UiSettings::default(); settings::init(settings.clone(), data.clone(), cx);
        let fonts = typography::register_fonts(cx); typography::init(settings.ui_font_family.clone(), settings.ui_font_size, fonts, cx);
        theme_library::init(data.clone(), cx); appearance::init(appearance::AppearanceMode::Dark, settings.theme_selection, settings.accent, settings.surface, cx);
        history::init(settings.git_history_columns, settings.git_history_column_widths, settings.git_history_column_order, settings.git_history_author_display, cx);
        composer::init(cx, settings.composer_send_behavior); terminal::panel::init(cx); app_menus::init(cx);
        let state = cx.new(|_| { let mut s = state::AppState::new(); s.connection = zeron_proto::view::ConnectionStatus::Ready; s.workspace_scope = Some(zeron_proto::WorkspaceScope::Development); s.local_device_id = Some(device.clone()); s.devices = vec![serde_json::from_value(serde_json::json!({"id":device,"name":"This device","platform":std::env::consts::OS,"lastSeenAt":null})).unwrap()]; s.chats = chats; s.spaces = spaces; s.selected_chat = Some("preview-fixture".into()); s.selected_space = Some("project".into()); s.auto_selected = true; s.chats_synced = true; s.spaces_synced = true; s });
        let window = cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(Bounds::new(gpui::point(px(12.),px(30.)),size(px(1100.),px(760.))))), ..Default::default() }, |_,cx| cx.new(|cx| shell::Shell::new(state.clone(),boot,cx))).unwrap();
        state.update(cx, |_,cx| cx.notify()); cx.activate(true);
        cx.spawn(async move |cx| {
            let run: anyhow::Result<()> = async {
                let mut vite_child = Some(vite_child); let mut api = Some(api);
                pause(cx,1200).await;
                state.update(cx, |s,cx| { s.receive_transcript_frame(zeron_doc::TranscriptFrame::Reset { reset: serde_json::from_value(serde_json::json!([
                    {"id":"user","role":"user","parts":[{"id":"text","kind":"text","text":"Let’s preview Fieldnotes while we work on the landing page."}],"createdAt":1788900000000_i64,"deviceId":"local"},
                    {"id":"assistant","role":"assistant","parts":[{"id":"text","kind":"text","text":"The development server is running. Open **Vite** in the browser tab to see your project.\n\nYour preview keeps the same address when the server restarts, and updates appear live as we edit."}],"createdAt":1788900001000_i64,"deviceId":"local","status":"complete"}
                ])).unwrap() },cx).unwrap(); });
                let (_, browser) = window.update(cx,|shell,w,cx|shell.fixture_open_browser(None,w,cx))?;
                browser.update(cx,|browser,cx|browser.watch_previews(handle.clone(),"preview-fixture".into(),cx));
                for _ in 0..100 { if browser.read_with(cx,|b,_|b.fixture_previews().services.len()==2) { break; } pause(cx,100).await; }
                let snapshot = browser.read_with(cx,|b,_|b.fixture_previews()); anyhow::ensure!(snapshot.services.len()==2,"did not discover Vite and API: {snapshot:?}");
                let stable = snapshot.services.iter().find(|s|s.name=="Vite").unwrap().url(snapshot.proxy_port);
                capture(&output,"preview-servers-dark")?;
                std::fs::write(output.join("ready.txt"),&stable)?;
                if std::env::var_os("ZERON_PREVIEW_AUTO_OPEN").is_some() {
                    // CI sends a real GPUI pointer sequence through hit testing.
                    let position = browser.read_with(cx,|b,_|b.fixture_preview_open_position()).ok_or_else(||anyhow::anyhow!("Open button was not laid out"))?;
                    gpui::AnyWindowHandle::from(window).update(cx,|_,w,cx| {
                        w.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { position,button:gpui::MouseButton::Left,click_count:1,..Default::default() }),cx);
                        w.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { position,button:gpui::MouseButton::Left,click_count:1,..Default::default() }),cx);
                    })?;
                }
                // A native mouse click on Open drives navigation; the runner can
                // inspect the initial screenshot before choosing its coordinates.
                for _ in 0..1200 { if browser.read_with(cx,|b,_|b.page.title=="Fieldnotes" && !b.page.loading) { break; } pause(cx,100).await; }
                if !browser.read_with(cx,|b,_|b.page.title=="Fieldnotes") { capture(&output,"preview-open-failed")?; }
                anyhow::ensure!(browser.read_with(cx,|b,_|b.page.title=="Fieldnotes"),"Open did not load the preview: {:?}",browser.read_with(cx,|b,_|b.page.clone()));
                pause(cx,1000).await; capture(&output,"preview-open-stable-url")?;
                std::fs::write(project.join("index.html"),html.replace("Fieldnotes</title>","Fieldnotes · Live update</title>").replace("Room for your next idea.","Ideas come to life."))?;
                for _ in 0..150 { if browser.read_with(cx,|b,_|b.page.title=="Fieldnotes · Live update") {break;} pause(cx,100).await; }
                anyhow::ensure!(browser.read_with(cx,|b,_|b.page.title=="Fieldnotes · Live update"),"Vite HMR did not reload through the stable proxy: {:?}", browser.read_with(cx,|b,_|b.page.clone()));
                capture(&output,"preview-live-update")?;
                drop(vite_child.take()); pause(cx,2600).await;
                anyhow::ensure!(browser.read_with(cx,|b,_|b.fixture_previews().services.len()==1),"stopped Vite remained in discovery");
                vite_child = Some(Child(std::process::Command::new("node").arg(vite_restart).args(["--host","127.0.0.1","--strictPort","--port",&second_port.to_string()]).current_dir(restart_root).stdout(std::process::Stdio::null()).spawn()?));
                for _ in 0..150 { if browser.read_with(cx,|b,_|b.fixture_previews().services.iter().any(|s|s.port==second_port)) {break;} pause(cx,100).await; }
                let after = browser.read_with(cx,|b,_|b.fixture_previews()); anyhow::ensure!(after.services.iter().any(|s|s.port==second_port && s.url(after.proxy_port)==stable),"port change lost stable identity");
                window.update(cx,|_,w,cx|browser.update(cx,|b,cx|b.navigate(&stable,w,cx)))?; pause(cx,2000).await; capture(&output,"preview-restarted-stable-url")?;
                drop(api.take()); pause(cx,2600).await;
                let (_,empty) = window.update(cx,|shell,w,cx|shell.fixture_open_browser(None,w,cx))?;
                empty.update(cx,|b,cx|b.watch_previews(handle.clone(),"preview-fixture".into(),cx)); pause(cx,800).await;
                anyhow::ensure!(empty.read_with(cx,|b,_|b.fixture_previews().services.len()==1),"stopped API remained in new tab");
                capture(&output,"preview-restarted-server-list")?;
                cx.update(|cx|appearance::set_mode(appearance::AppearanceMode::Light,cx)); pause(cx,700).await; capture(&output,"preview-servers-light")?;
                std::fs::write(output.join("result.txt"),format!("PASS: live process discovery, native Open click, stable HTTP URL, Vite HMR, disappearance, restart {first_port} → {second_port}, persistent identity, new-tab scoping\n{stable}\n"))?;
                pause(cx,1500).await; drop(vite_child); Ok(())
            }.await;
            if let Err(error) = run { eprintln!("Preview fixture failed: {error:#}"); *result.lock().unwrap()=Some(format!("{error:#}")); }
            let _ = window.update(cx,|shell,w,cx|shell.fixture_blur_browser(w,cx)); pause(cx,200).await;
            drop(state); let _ = window.update(cx,|_,w,_|w.remove_window()); pause(cx,100).await;
            cx.update(|cx|cx.quit());
        }).detach();
    });
    runtime.block_on(core.shutdown());
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
