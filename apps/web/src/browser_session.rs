//! Browser cookie/session lifecycle for the shared Shell.
//!
//! This module deliberately owns no chat reducer. It discovers an authenticated
//! browser session, lets the user choose a DeviceRoom, and attaches that typed
//! transport to the one `zeron_ui::state::AppState` rendered by the Shell.

/// Coordinates browser requests without depending on browser APIs, so the
/// cancellation and activity policy is covered by the lifecycle harness.
#[derive(Default, Debug)]
pub struct LifecycleCoordinator {
    epoch: u64,
    next_request: u64,
    pending: Vec<u64>,
    last_activity_at: Option<f64>,
}

impl LifecycleCoordinator {
    pub fn begin_epoch(&mut self) -> (u64, Vec<u64>) {
        self.epoch = self.epoch.wrapping_add(1);
        (self.epoch, std::mem::take(&mut self.pending))
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch
    }

    pub fn begin_request(&mut self, epoch: u64) -> Option<u64> {
        if epoch != self.epoch {
            return None;
        }
        self.next_request = self.next_request.wrapping_add(1);
        self.pending.push(self.next_request);
        Some(self.next_request)
    }

    pub fn finish_request(&mut self, request: u64) {
        self.pending.retain(|pending| *pending != request);
    }

    pub fn should_report_activity(&mut self, now: f64, interval_ms: f64) -> bool {
        if self
            .last_activity_at
            .is_some_and(|last| now - last < interval_ms)
        {
            return false;
        }
        self.last_activity_at = Some(now);
        true
    }
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        rc::Rc,
        time::Duration,
    };

    use gpui::{AnyWindowHandle, App, AppContext as _, Entity};
    use serde::Deserialize;
    use wasm_bindgen::{JsCast, closure::Closure};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        AbortController, Event, Request, RequestCredentials, RequestInit, Response, Window,
    };
    use zeron_ui::state::{AppState, EngineHandle};
    use zeron_ui::{EngineBootConfig, shell};

    use crate::rpc::connection::{ConnectionEpoch, ConnectionEpochs, connect_client};

    #[derive(Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SessionDto {
        authenticated: bool,
        #[serde(default)]
        owner_id: Option<String>,
        #[serde(default)]
        organization_id: Option<String>,
        #[serde(default)]
        csrf_token: Option<String>,
    }

    #[derive(Clone, Deserialize)]
    struct DeviceDto {
        id: String,
        #[serde(default)]
        name: Option<String>,
        online: bool,
    }

    #[derive(Deserialize)]
    struct DevicesDto {
        devices: Vec<DeviceDto>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LoginDto {
        authorization_url: String,
    }

    #[derive(Deserialize)]
    struct ActivityDto {
        authenticated: bool,
    }

    const REQUEST_TIMEOUT_MS: i32 = 15_000;
    const ACTIVITY_INTERVAL_MS: f64 = 60_000.0;

    enum RequestError {
        Expired,
        Cancelled,
        Failed(String),
    }

    impl RequestError {
        fn message(&self) -> &str {
            match self {
                Self::Expired => "Your browser session has expired",
                Self::Cancelled => "Browser session request was cancelled",
                Self::Failed(message) => message,
            }
        }
    }

    #[derive(Clone)]
    enum Action {
        Check,
        Login,
        DevLogin,
        SelectDevice(String),
        Logout,
        Activity,
        SwitchDevice,
    }

    enum Screen {
        Loading,
        SignedOut,
        Devices(Vec<DeviceDto>),
        Connecting(String),
        Connected,
        Failed(String),
    }

    /// Keeps browser authentication separate from the engine-facing AppState.
    pub struct BrowserSession {
        state: RefCell<Entity<AppState>>,
        boot: EngineBootConfig,
        window: AnyWindowHandle,
        epochs: RefCell<ConnectionEpochs>,
        requests: RefCell<super::LifecycleCoordinator>,
        controllers: RefCell<Vec<(u64, AbortController)>>,
        session: RefCell<Option<SessionDto>>,
        screen: RefCell<Screen>,
        actions: Rc<RefCell<VecDeque<Action>>>,
        handlers: RefCell<Vec<Closure<dyn FnMut(Event)>>>,
        input_handlers: RefCell<Vec<Closure<dyn FnMut(Event)>>>,
    }

    impl BrowserSession {
        pub fn start(
            state: Entity<AppState>,
            boot: EngineBootConfig,
            window: AnyWindowHandle,
            cx: &mut App,
        ) -> Rc<Self> {
            let session = Rc::new(Self {
                state: RefCell::new(state),
                boot,
                window,
                epochs: RefCell::new(ConnectionEpochs::default()),
                requests: RefCell::new(super::LifecycleCoordinator::default()),
                controllers: RefCell::new(Vec::new()),
                session: RefCell::new(None),
                screen: RefCell::new(Screen::Loading),
                actions: Rc::new(RefCell::new(VecDeque::new())),
                handlers: RefCell::new(Vec::new()),
                input_handlers: RefCell::new(Vec::new()),
            });
            session.install_activity_handlers();
            session.render();
            let runner = session.clone();
            cx.spawn(async move |cx| runner.run(cx).await).detach();
            session
        }

        /// Shared Shell account actions are browser cookie/session operations, not
        /// native-engine runtime transitions.
        pub fn handle_external_lifecycle_action(&self, action: shell::ExternalLifecycleAction) {
            self.queue(match action {
                shell::ExternalLifecycleAction::SwitchDevice => Action::SwitchDevice,
                shell::ExternalLifecycleAction::SignOut => Action::Logout,
                shell::ExternalLifecycleAction::Retry => Action::Check,
            });
        }

        async fn run(self: Rc<Self>, cx: &mut gpui::AsyncApp) {
            self.start_check(cx);
            loop {
                while let Some(action) = self.actions.borrow_mut().pop_front() {
                    self.handle(action, cx);
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }

        fn handle(self: &Rc<Self>, action: Action, cx: &mut gpui::AsyncApp) {
            match action {
                Action::Check => self.start_check(cx),
                Action::Login => self.start_login(cx),
                Action::DevLogin => self.start_dev_login(cx),
                Action::SelectDevice(device_id) => self.start_connect(device_id, cx),
                Action::Logout => self.start_logout(cx),
                Action::Activity => self.start_activity(cx),

                Action::SwitchDevice => self.start_switch_device(cx),
            }
        }

        fn start_check(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            let request_epoch = self.begin_request_epoch();
            let epoch = self.epochs.borrow_mut().begin_auth();
            self.set_screen(Screen::Loading);
            let session = self.clone();
            cx.spawn(async move |cx| session.check(epoch, request_epoch, cx).await)
                .detach();
        }

        async fn check(
            self: Rc<Self>,
            epoch: ConnectionEpoch,
            request_epoch: u64,
            cx: &mut gpui::AsyncApp,
        ) {
            let result = self
                .get_json::<SessionDto>(request_epoch, "/api/browser/session")
                .await;
            if !self.epochs.borrow().is_current(epoch) {
                return;
            }
            match result {
                Ok(session) if session.authenticated => {
                    if session.owner_id.as_deref().unwrap_or_default().is_empty()
                        || session.csrf_token.as_deref().unwrap_or_default().is_empty()
                    {
                        self.fail("The browser session response was incomplete", cx)
                            .await;
                        return;
                    }
                    *self.session.borrow_mut() = Some(session);
                    self.load_devices(epoch, request_epoch, cx).await;
                }
                Ok(_) | Err(RequestError::Expired) => self.expire(cx).await,
                Err(RequestError::Cancelled) => {}
                Err(error) => self.fail(error.message(), cx).await,
            }
        }

        async fn load_devices(
            &self,
            epoch: ConnectionEpoch,
            request_epoch: u64,
            cx: &mut gpui::AsyncApp,
        ) {
            match self
                .get_json::<DevicesDto>(request_epoch, "/api/browser/devices")
                .await
            {
                Ok(devices) if self.epochs.borrow().is_current(epoch) => {
                    self.set_screen(Screen::Devices(devices.devices));
                }
                Ok(_) | Err(RequestError::Cancelled) => {}
                Err(RequestError::Expired) if self.epochs.borrow().is_current(epoch) => {
                    self.expire(cx).await
                }
                Err(error) if self.epochs.borrow().is_current(epoch) => {
                    self.fail(error.message(), cx).await
                }
                Err(_) => {}
            }
        }

        fn start_login(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            let request_epoch = self.begin_request_epoch();
            self.set_screen(Screen::Loading);
            let session = self.clone();
            cx.spawn(async move |cx| {
                match session
                    .post_json::<LoginDto>(request_epoch, "/api/browser/login", None)
                    .await
                {
                    Ok(login) => {
                        if let Some(window) = web_sys::window() {
                            let _ = window.location().set_href(&login.authorization_url);
                        }
                    }
                    Err(RequestError::Cancelled) => {}
                    Err(RequestError::Expired) => session.expire(cx).await,
                    Err(error) => session.set_screen(Screen::Failed(error.message().into())),
                }
            })
            .detach();
        }

        fn start_switch_device(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            let request_epoch = self.begin_request_epoch();
            let epoch = self.epochs.borrow_mut().begin_auth();
            self.set_screen(Screen::Loading);
            let session = self.clone();
            cx.spawn(async move |cx| {
                session.detach_engine(cx).await;
                if session.epochs.borrow().is_current(epoch) {
                    session.check(epoch, request_epoch, cx).await;
                }
            })
            .detach();
        }

        fn start_dev_login(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            if !dev_login_enabled() {
                return;
            }
            let request_epoch = self.begin_request_epoch();
            self.set_screen(Screen::Loading);
            let session = self.clone();
            cx.spawn(async move |cx| {
                match session
                    .post_json::<serde_json::Value>(request_epoch, "/api/browser/dev-login", None)
                    .await
                {
                    Ok(_) => session.start_check(cx),
                    Err(RequestError::Cancelled) => {}
                    Err(RequestError::Expired) => session.expire(cx).await,
                    Err(error) => session.set_screen(Screen::Failed(error.message().into())),
                }
            })
            .detach();
        }

        fn start_connect(self: &Rc<Self>, device_id: String, cx: &mut gpui::AsyncApp) {
            self.begin_request_epoch();
            let epoch = self.epochs.borrow_mut().begin_socket();
            self.set_screen(Screen::Connecting(device_id.clone()));
            let session = self.clone();
            cx.spawn(async move |cx| session.connect(epoch, device_id, cx).await)
                .detach();
        }

        async fn connect(
            self: Rc<Self>,
            epoch: ConnectionEpoch,
            device_id: String,
            cx: &mut gpui::AsyncApp,
        ) {
            self.detach_engine(cx).await;
            if !self.epochs.borrow().is_current(epoch) {
                return;
            }
            let connected = match connect_client(epoch, &device_id).await {
                Ok(connected) => connected,
                Err(error) => {
                    if self.epochs.borrow().is_current(epoch) {
                        self.set_screen(Screen::Failed(error));
                    }
                    return;
                }
            };
            let url = connected.url().to_string();
            let handle =
                match EngineHandle::from_connected_client(connected.into_client(), url).await {
                    Ok(handle) => handle,
                    Err(error) => {
                        if self.epochs.borrow().is_current(epoch) {
                            self.set_screen(Screen::Failed(error.to_string()));
                        }
                        return;
                    }
                };
            if !self.epochs.borrow().is_current(epoch) {
                handle.shutdown().await;
                return;
            }
            let mut closed = handle.client().watch_closed();

            self.state
                .borrow()
                .clone()
                .update(cx, |state, cx| state.attach_engine(handle, cx));
            self.set_screen(Screen::Connected);

            let watcher = self.clone();
            cx.spawn(async move |cx| {
                if closed.changed().await.is_ok()
                    && *closed.borrow()
                    && watcher.epochs.borrow().is_current(epoch)
                {
                    watcher.detach_engine(cx).await;
                    watcher.set_screen(Screen::Failed(
                        "The remote device disconnected. Nothing was retried.".into(),
                    ));
                }
            })
            .detach();
        }

        fn start_logout(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            let request_epoch = self.begin_request_epoch();
            let epoch = self.epochs.borrow_mut().begin_auth();
            let csrf = self
                .session
                .borrow()
                .as_ref()
                .and_then(|session| session.csrf_token.clone());
            self.set_screen(Screen::Loading);
            let session = self.clone();
            cx.spawn(async move |cx| {
                session.detach_engine(cx).await;
                let result = session
                    .post_json::<serde_json::Value>(
                        request_epoch,
                        "/api/browser/logout",
                        csrf.as_deref(),
                    )
                    .await;
                if !session.epochs.borrow().is_current(epoch) {
                    return;
                }
                match result {
                    Ok(_) | Err(RequestError::Expired) => {
                        *session.session.borrow_mut() = None;
                        session.set_screen(Screen::SignedOut);
                    }
                    Err(RequestError::Cancelled) => {}
                    Err(error) => session.set_screen(Screen::Failed(error.message().into())),
                }
            })
            .detach();
        }

        fn start_activity(self: &Rc<Self>, cx: &mut gpui::AsyncApp) {
            let csrf = self
                .session
                .borrow()
                .as_ref()
                .and_then(|session| session.csrf_token.clone());
            let Some(csrf) = csrf else { return };
            let request_epoch = {
                let mut requests = self.requests.borrow_mut();
                if !requests.should_report_activity(js_sys::Date::now(), ACTIVITY_INTERVAL_MS) {
                    return;
                }
                requests.current_epoch()
            };
            let session = self.clone();
            cx.spawn(async move |cx| {
                match session
                    .post_json::<ActivityDto>(request_epoch, "/api/browser/activity", Some(&csrf))
                    .await
                {
                    Ok(activity) if !activity.authenticated => session.expire(cx).await,
                    Ok(_) | Err(RequestError::Cancelled) => {}
                    Err(RequestError::Expired) => session.expire(cx).await,
                    Err(_) => {}
                }
            })
            .detach();
        }

        fn begin_request_epoch(&self) -> u64 {
            let (epoch, cancelled) = self.requests.borrow_mut().begin_epoch();
            let mut controllers = self.controllers.borrow_mut();
            controllers.retain(|(request, controller)| {
                if cancelled.contains(request) {
                    controller.abort();
                    false
                } else {
                    true
                }
            });
            epoch
        }

        async fn get_json<T: for<'de> Deserialize<'de>>(
            &self,
            request_epoch: u64,
            path: &str,
        ) -> Result<T, RequestError> {
            self.request_json(request_epoch, path, "GET", None).await
        }

        async fn post_json<T: for<'de> Deserialize<'de>>(
            &self,
            request_epoch: u64,
            path: &str,
            csrf: Option<&str>,
        ) -> Result<T, RequestError> {
            self.request_json(request_epoch, path, "POST", csrf).await
        }

        async fn request_json<T: for<'de> Deserialize<'de>>(
            &self,
            request_epoch: u64,
            path: &str,
            method: &str,
            csrf: Option<&str>,
        ) -> Result<T, RequestError> {
            let request = {
                let mut requests = self.requests.borrow_mut();
                let Some(request) = requests.begin_request(request_epoch) else {
                    return Err(RequestError::Cancelled);
                };
                request
            };
            let controller = AbortController::new().map_err(|_| {
                RequestError::Failed("Cannot create browser request cancellation".into())
            })?;
            self.controllers
                .borrow_mut()
                .push((request, controller.clone()));
            let result = request_json(path, method, csrf, &controller).await;
            self.controllers
                .borrow_mut()
                .retain(|(pending, _)| *pending != request);
            self.requests.borrow_mut().finish_request(request);
            if self.requests.borrow().current_epoch() != request_epoch {
                Err(RequestError::Cancelled)
            } else {
                result
            }
        }

        fn install_activity_handlers(self: &Rc<Self>) {
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            for event in ["keydown", "pointerdown"] {
                let actions = self.actions.clone();
                let handler = Closure::new(move |_: Event| {
                    actions.borrow_mut().push_back(Action::Activity);
                });
                let _ = document
                    .add_event_listener_with_callback(event, handler.as_ref().unchecked_ref());
                self.input_handlers.borrow_mut().push(handler);
            }
        }

        async fn detach_engine(&self, cx: &mut gpui::AsyncApp) {
            let state = self.state.borrow().clone();
            let old = state.update(cx, |state, cx| state.detach_engine(cx));
            if let Some(handle) = old {
                handle.shutdown().await;
            }
            self.replace_shell(cx);
        }

        fn replace_shell(&self, cx: &mut gpui::AsyncApp) {
            let boot = self.boot.clone();
            let state = self.window.update(cx, move |_, window, cx| {
                let state = cx.new(|_| AppState::new());
                window.replace_root(cx, |_, cx| shell::Shell::new(state.clone(), boot, cx));
                state
            });
            if let Ok(state) = state {
                *self.state.borrow_mut() = state;
            }
        }

        async fn expire(&self, cx: &mut gpui::AsyncApp) {
            self.begin_request_epoch();
            self.epochs.borrow_mut().begin_auth();
            *self.session.borrow_mut() = None;
            self.detach_engine(cx).await;
            self.set_screen(Screen::SignedOut);
        }

        async fn fail(&self, message: &str, cx: &mut gpui::AsyncApp) {
            self.detach_engine(cx).await;
            self.set_screen(Screen::Failed(message.into()));
        }

        fn set_screen(&self, screen: Screen) {
            *self.screen.borrow_mut() = screen;
            self.render();
        }

        fn queue(&self, action: Action) {
            self.actions.borrow_mut().push_back(action);
        }

        fn render(&self) {
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            let Some(body) = document.body() else {
                return;
            };
            let root = match document.get_element_by_id("comet-browser-session") {
                Some(root) => root,
                None => {
                    let Ok(root) = document.create_element("div") else {
                        return;
                    };
                    root.set_id("comet-browser-session");
                    let _ = body.append_child(&root);
                    root
                }
            };

            if matches!(&*self.screen.borrow(), Screen::Connected) {
                root.remove();
                return;
            }
            self.handlers.borrow_mut().clear();
            root.set_text_content(None);
            let _ = root.set_attribute(
            "style",
            "position:fixed;inset:0;z-index:10;display:flex;align-items:center;justify-content:center;background:rgba(24,24,27,.76);color:#fafafa;font:14px system-ui,sans-serif",
        );
            let Ok(card) = document.create_element("div") else {
                return;
            };
            let _ = card.set_attribute(
            "style",
            "width:min(420px,calc(100vw - 32px));padding:24px;border:1px solid #3f3f46;border-radius:12px;background:#18181b;box-shadow:0 20px 50px rgba(0,0,0,.35)",
        );
            match &*self.screen.borrow() {
                Screen::Loading => self.text(
                    &document,
                    &card,
                    "Connecting…",
                    "Checking your browser session.",
                ),
                Screen::SignedOut => {
                    self.text(
                        &document,
                        &card,
                        "Sign in",
                        "Sign in to choose a remote device.",
                    );
                    self.button(&document, &card, "Sign in", Action::Login, false);
                    if dev_login_enabled() {
                        self.button(
                            &document,
                            &card,
                            "Development sign in",
                            Action::DevLogin,
                            false,
                        );
                    }
                }
                Screen::Devices(devices) => {
                    self.text(
                        &document,
                        &card,
                        "Choose a device",
                        "Your browser connects only to a remote device.",
                    );
                    if devices.is_empty() {
                        self.note(
                            &document,
                            &card,
                            "No registered devices are available for this account.",
                        );
                    }
                    for device in devices {
                        let label = device.name.as_deref().unwrap_or(&device.id);
                        let label = if device.online {
                            format!("{label} · Online")
                        } else {
                            format!("{label} · Offline")
                        };
                        self.button(
                            &document,
                            &card,
                            &label,
                            Action::SelectDevice(device.id.clone()),
                            !device.online,
                        );
                    }
                    self.button(&document, &card, "Refresh", Action::Check, false);
                    self.button(&document, &card, "Log out", Action::Logout, false);
                }
                Screen::Connecting(device) => self.text(
                    &document,
                    &card,
                    "Connecting…",
                    &format!("Opening the remote DeviceRoom for {device}."),
                ),
                Screen::Connected => return,
                Screen::Failed(error) => {
                    self.text(&document, &card, "Connection failed", error);
                    self.button(&document, &card, "Try again", Action::Check, false);
                }
            }
            let _ = root.append_child(&card);
        }

        fn text(
            &self,
            document: &web_sys::Document,
            parent: &web_sys::Element,
            title: &str,
            detail: &str,
        ) {
            let Ok(heading) = document.create_element("h1") else {
                return;
            };
            heading.set_text_content(Some(title));
            let _ = heading.set_attribute("style", "margin:0 0 8px;font-size:18px");
            let _ = parent.append_child(&heading);
            self.note(document, parent, detail);
        }

        fn note(&self, document: &web_sys::Document, parent: &web_sys::Element, text: &str) {
            let Ok(note) = document.create_element("p") else {
                return;
            };
            note.set_text_content(Some(text));
            let _ = note.set_attribute("style", "margin:0 0 16px;color:#c4c4ca;line-height:1.45");
            let _ = parent.append_child(&note);
        }

        fn button(
            &self,
            document: &web_sys::Document,
            parent: &web_sys::Element,
            label: &str,
            action: Action,
            disabled: bool,
        ) {
            let Ok(button) = document.create_element("button") else {
                return;
            };
            button.set_text_content(Some(label));
            let _ = button.set_attribute(
            "style",
            "display:block;width:100%;margin:8px 0 0;padding:9px 12px;border:1px solid #52525b;border-radius:7px;background:#27272a;color:#fafafa;text-align:left;cursor:pointer",
        );
            if disabled {
                let _ = button.set_attribute("disabled", "");
                let _ = button.set_attribute("style", "display:block;width:100%;margin:8px 0 0;padding:9px 12px;border:1px solid #3f3f46;border-radius:7px;background:#202024;color:#71717a;text-align:left");
            } else {
                let actions = self.actions.clone();
                let handler =
                    Closure::new(move |_: Event| actions.borrow_mut().push_back(action.clone()));
                let _ = button
                    .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref());
                self.handlers.borrow_mut().push(handler);
            }
            let _ = parent.append_child(&button);
        }
    }

    fn dev_login_enabled() -> bool {
        let Some(location) = web_sys::window().map(|window| window.location()) else {
            return false;
        };
        let host = location.hostname().unwrap_or_default();
        let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]");
        loopback
            && location
                .search()
                .unwrap_or_default()
                .trim_start_matches('?')
                .split('&')
                .any(|pair| pair == "comet-dev-login=1")
    }

    struct RequestTimeout {
        window: Window,
        id: i32,
        expired: Rc<Cell<bool>>,
        _callback: Closure<dyn FnMut()>,
    }

    impl RequestTimeout {
        fn new(controller: AbortController) -> Result<Self, RequestError> {
            let window = web_sys::window()
                .ok_or_else(|| RequestError::Failed("Browser window unavailable".into()))?;
            let expired = Rc::new(Cell::new(false));
            let timed_out = expired.clone();
            let callback = Closure::new(move || {
                timed_out.set(true);
                controller.abort();
            });
            let id = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.as_ref().unchecked_ref(),
                    REQUEST_TIMEOUT_MS,
                )
                .map_err(|_| RequestError::Failed("Cannot start browser request timeout".into()))?;
            Ok(Self {
                window,
                id,
                expired,
                _callback: callback,
            })
        }

        fn timed_out(&self) -> bool {
            self.expired.get()
        }
    }

    impl Drop for RequestTimeout {
        fn drop(&mut self) {
            self.window.clear_timeout_with_handle(self.id);
        }
    }

    async fn request_json<T: for<'de> Deserialize<'de>>(
        path: &str,
        method: &str,
        csrf: Option<&str>,
        controller: &AbortController,
    ) -> Result<T, RequestError> {
        let options = RequestInit::new();
        options.set_method(method);
        options.set_credentials(RequestCredentials::SameOrigin);
        options.set_signal(Some(&controller.signal()));
        let request = Request::new_with_str_and_init(path, &options)
            .map_err(|_| RequestError::Failed("Cannot create browser session request".into()))?;
        if let Some(csrf) = csrf {
            request.headers().set("X-CSRF-Token", csrf).map_err(|_| {
                RequestError::Failed("Cannot add browser session credentials".into())
            })?;
        }
        let window = web_sys::window()
            .ok_or_else(|| RequestError::Failed("Browser window unavailable".into()))?;
        let timeout = RequestTimeout::new(controller.clone())?;
        let fetched = JsFuture::from(window.fetch_with_request(&request)).await;
        if timeout.timed_out() {
            return Err(RequestError::Failed(
                "Browser session request timed out".into(),
            ));
        }
        let response = fetched
            .map_err(|_| RequestError::Failed("Browser session request failed".into()))?
            .dyn_into::<Response>()
            .map_err(|_| RequestError::Failed("Invalid browser session response".into()))?;
        if !response.ok() {
            return Err(if response.status() == 401 {
                RequestError::Expired
            } else {
                RequestError::Failed(format!(
                    "Browser session request failed ({})",
                    response.status()
                ))
            });
        }
        let body = response
            .text()
            .map_err(|_| RequestError::Failed("Cannot read browser session response".into()))?;
        let text = JsFuture::from(body).await;
        if timeout.timed_out() {
            return Err(RequestError::Failed(
                "Browser session request timed out".into(),
            ));
        }
        let text = text
            .map_err(|_| RequestError::Failed("Cannot read browser session response".into()))?
            .as_string()
            .ok_or_else(|| RequestError::Failed("Invalid browser session response".into()))?;
        serde_json::from_str(&text)
            .map_err(|_| RequestError::Failed("Invalid browser session response".into()))
    }
}

#[cfg(target_arch = "wasm32")]
pub use browser::BrowserSession;
