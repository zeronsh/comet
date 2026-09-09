use super::{BrowserEvent, BrowserSurface};
use crate::{icons, surface_chrome, theme::Theme};
use gpui::{
    AnyElement, Context, Focusable, IntoElement, KeyDownEvent, MouseButton, Render, Window, div,
    prelude::*, px,
};

fn button(
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    enabled: bool,
    theme: &Theme,
    _cx: &mut Context<BrowserSurface>,
) -> gpui::Stateful<gpui::Div> {
    // Match Files/History chrome, including focus-preserving mouse-down and
    // tooltips. Disabled controls have neither a pointer cursor nor a handler.
    crate::files::toolbar_button(id, label)
        .when(!enabled, |el| el.cursor_default().opacity(0.35))
        .child(
            icons::icon(glyph)
                .size(px(surface_chrome::ICON_SIZE))
                .text_color(theme.text_muted),
        )
}

impl BrowserSurface {
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "linux")]
        if self.linux_menu_key(&event.keystroke.key, cx) {
            cx.stop_propagation();
            return;
        }
        let address_focused = self.address.focus_handle(cx).is_focused(window);
        #[cfg(target_os = "linux")]
        if !address_focused
            && self.focus.is_focused(window)
            && self.presentation == super::model::Presentation::Live
        {
            if event.prefer_character_input
                && let Some(text) = &event.keystroke.key_char
            {
                if let Some(native) = &self.native {
                    native.command(serde_json::json!({"cmd":"commit","text":text}));
                }
                cx.stop_propagation();
                return;
            }
            if event.keystroke.modifiers.control && !event.keystroke.modifiers.alt {
                if let Some(native) = &self.native {
                    match event.keystroke.key.as_str() {
                        "c" | "x" => {
                            native.command(serde_json::json!({"cmd":if event.keystroke.key=="c" {"copy"} else {"cut"}}));
                            cx.stop_propagation();
                            return;
                        }
                        "v" => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                native.command(serde_json::json!({"cmd":"text","text":text}));
                            }
                            cx.stop_propagation();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            self.linux_key(&event.keystroke, true);
            cx.stop_propagation();
            return;
        }
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        if address_focused && !primary && !mods.alt {
            match key {
                "enter" if !mods.shift => self.submit(window, cx),
                "escape" => {
                    let url = self.page.url.clone().unwrap_or_default();
                    self.address.update(cx, |input, cx| input.set_text(url, cx));
                    self.address_edited = false;
                    self.validation = None;
                    window.focus(&self.focus, cx);
                }
                _ => return,
            }
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn preview_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = &self.previews;
        let available = snapshot.error.is_none();
        let subtitle = if snapshot.remote {
            "Running on your device"
        } else {
            "Running locally"
        };
        let mut content = div()
            .w_full()
            .max_w(px(280.0))
            .flex_shrink_0()
            .my_auto()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .mb(px(4.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(subtitle),
            );
        for service in &snapshot.services {
            let url = service.url(snapshot.proxy_port);
            let row_url = url.clone();
            let border_strong = theme.border_strong;
            let label = if snapshot.remote {
                format!("{} · localhost:{}", service.device_name, service.port)
            } else {
                format!("localhost:{}", service.port)
            };
            content = content.child(
                div()
                    .id(gpui::SharedString::from(format!("preview-row-{}", service.id)))
                    .w_full()
                    .h(px(56.0))
                    .px(px(14.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.02))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .when(available, |el| {
                        el.cursor_pointer()
                            .hover(move |style| {
                                style
                                    .bg(crate::theme::ink(0.05))
                                    .border_color(border_strong)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(&row_url, window, cx)
                            }))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .truncate()
                                    .child(service.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .map(|el| {
                                #[cfg(feature = "browser-fixture")]
                                if snapshot
                                    .services
                                    .first()
                                    .is_some_and(|first| first.id == service.id)
                                {
                                    let position = self.fixture_preview_open.clone();
                                    return el.on_children_prepainted(move |bounds, _, _| {
                                        position.set(bounds.first().map(|bounds| bounds.center()));
                                    });
                                }
                                el
                            })
                            .id(gpui::SharedString::from(format!(
                                "open-preview-{}",
                                service.id
                            )))
                            .h(px(28.0))
                            .px(px(6.0))
                            .flex_shrink_0()
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .role(gpui::Role::Button)
                            .aria_label(format!("Open {} preview", service.name))
                            .when(available, |el| {
                                el.cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::ink(0.05)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.navigate(&url, window, cx)
                                    }))
                            })
                            .when(!available, |el| el.opacity(0.4))
                            .child("Open"),
                    ),
            );
        }
        if snapshot.services.is_empty() {
            let message = if self.previews_loading {
                "Looking for dev servers…"
            } else if snapshot.remote {
                "Start a dev server in this project on your other device. Its preview will appear here when that device is online."
            } else {
                "Start a dev server in this project. It will appear here automatically, ready to open."
            };
            content = content.child(
                div()
                    .p(px(16.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(message),
            );
        }
        if let Some(error) = &snapshot.error {
            content = content.child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .line_height(px(17.0))
                    .text_color(theme.text_muted)
                    .child(error.clone()),
            );
        }
        content = content.child(
            div()
                .id("preview-enter-address")
                .mt(px(8.0))
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted)
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label("Enter a website address")
                .on_click(cx.listener(|this, _, window, cx| this.focus_address(window, cx)))
                .child("Or enter a website address"),
        );
        div()
            .id("browser-previews")
            .size_full()
            .overflow_y_scroll()
            .p(px(16.0))
            .flex()
            .flex_col()
            .items_center()
            .child(content)
            .into_any_element()
    }

    fn empty_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.page.url.is_none() && self.previews_task.is_some() {
            return self.preview_body(theme, cx);
        }
        let external = !cfg!(any(target_os = "macos", target_os = "linux"));
        let has_error = self.page.error.is_some();
        let title = if has_error {
            "Couldn’t load this page"
        } else if external && self.page.url.is_some() {
            "Opened in your browser"
        } else {
            "Preview your work"
        };
        let description = if let Some(error) = &self.page.error {
            error.clone()
        } else if external {
            "Open a website or local app in your default browser. Embedded browsing is available on macOS and Linux.".into()
        } else {
            "Preview your local app or keep a website beside your conversation.".into()
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(300.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .size(px(44.0))
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised.opacity(0.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icons::icon(icons::GLOBE)
                                    .size(px(22.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_center()
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted)
                            .child(description),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .id("browser-empty-action")
                            .h(px(28.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised)
                            .cursor_pointer()
                            .role(gpui::Role::Button)
                            .aria_label(if has_error {
                                "Retry page"
                            } else {
                                "Enter an address"
                            })
                            .hover(|style| style.bg(crate::theme::wash(0.10)))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if has_error {
                                    this.reload(cx);
                                } else {
                                    this.focus_address(window, cx);
                                }
                            }))
                            .child(if has_error {
                                "Try again"
                            } else {
                                "Enter an address"
                            })
                            .when(!has_error, |el| {
                                el.child(
                                    div()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_faint)
                                        .child(if cfg!(target_os = "macos") {
                                            "⌘L"
                                        } else {
                                            "Ctrl L"
                                        }),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let focused = self.address.focus_handle(cx).is_focused(window);
        let external = !cfg!(any(target_os = "macos", target_os = "linux"));
        let has_page = self.page.url.is_some();
        let back = button(
            "browser-back",
            "Back",
            icons::ARROW_LEFT,
            self.page.can_back,
            &theme,
            cx,
        )
        .when(self.page.can_back, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(false)))
        });
        let forward = button(
            "browser-forward",
            "Forward",
            icons::ARROW_RIGHT,
            self.page.can_forward,
            &theme,
            cx,
        )
        .when(self.page.can_forward, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(true)))
        });
        let reload = button(
            "browser-reload",
            "Reload page",
            icons::REFRESH,
            has_page,
            &theme,
            cx,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
        });
        let address = div()
            .id("browser-address")
            .flex_1()
            .min_w_0()
            .h(px(26.0))
            .px(px(7.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if self.validation.is_some() {
                theme.danger
            } else if focused {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_raised.opacity(0.65))
            .flex()
            .items_center()
            .gap(px(6.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    #[cfg(target_os = "macos")]
                    if let Some(native) = &this.native {
                        native.focus_chrome();
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = this;
                }),
            )
            .child(
                icons::icon(icons::GLOBE)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(18.0))
                    .overflow_hidden()
                    .child(self.address.clone()),
            )
            .when(focused || self.address_edited, |el| {
                el.child(
                    div()
                        .id("browser-go")
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("Go to address")
                        .hover(|s| s.bg(crate::theme::wash(0.10)))
                        .on_mouse_down(MouseButton::Left, |_, w, _| w.prevent_default())
                        .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx)))
                        .child(
                            icons::icon(icons::RETURN)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
            });
        let open = button(
            "browser-external",
            "Open in default browser",
            icons::ARROW_UP_RIGHT,
            has_page,
            &theme,
            cx,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.open_external(cx)))
        });
        let toolbar = surface_chrome::toolbar(&theme)
            .when(!external, |el| el.child(back).child(forward).child(reload))
            .child(address)
            .child(open);

        let body = div()
            .id("browser-page")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.bg);
        #[cfg(target_os = "macos")]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(&theme, cx))
            } else {
                let native = native.handle();
                body.child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let native = std::rc::Rc::downgrade(&native);
                            let mask = window.content_mask().bounds;
                            let dragging = cx.has_active_drag();
                            window.on_present(move || {
                                if let Some(native) = native.upgrade() {
                                    native.borrow_mut().sync(bounds, mask, dragging);
                                }
                            });
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
            }
        } else {
            body.child(self.empty_body(&theme, cx))
        };
        #[cfg(target_os = "linux")]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(&theme, cx))
            } else {
                let image = native.image.clone();
                let image_scale = native.image_scale;
                let entity = cx.entity().downgrade();
                body.child(gpui::canvas(|_,_,_| (), move |bounds,_,window,cx| {
                    let scale = window.scale_factor();
                    if let Some(view)=entity.upgrade() {
                        let focus=view.read(cx).focus.clone();
                        window.handle_input(&focus,gpui::ElementInputHandler::new(bounds,view),cx);
                    }
                    let _ = entity.update(cx, |this,_| {
                        if let Some(native)=&mut this.native { native.sync(bounds,scale); }
                    });
                    let capture=entity.clone();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent,phase,_,cx| {
                        if phase==gpui::DispatchPhase::Bubble && !bounds.contains(&event.position) && !cx.has_active_drag() {
                            let _=capture.update(cx,|this,_| {
                                if this.native.as_ref().is_some_and(|n|n.pressed.get().is_some()) {
                                    this.linux_pointer("move",event.position,event.pressed_button,event.modifiers);
                                }
                            });
                        }
                    });
                    if let Some(image)=&image {
                        if let Some(bytes)=image.as_bytes(0).filter(|b| b.len() >= 4) {
                            let color=gpui::rgb(((bytes[2] as u32)<<16)|((bytes[1] as u32)<<8)|bytes[0] as u32);
                            window.paint_quad(gpui::fill(bounds,color));
                        }
                        let dimensions=image.size(0);
                        // Keep the previous frame at its original scale while
                        // WebKit reflows; clipping never stretches the page.
                        let viewport=gpui::Bounds::new(bounds.origin,gpui::size(px(dimensions.width.0 as f32/image_scale),px(dimensions.height.0 as f32/image_scale)));
                        let _=window.paint_image(viewport,gpui::Corners::default(),image.clone(),0,false);
                    }
                }).absolute().inset_0())
                .on_mouse_down(MouseButton::Left,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {
                    if !cx.has_active_drag() { w.focus(&this.focus,cx); this.linux_pointer("down",event.position,Some(event.button),event.modifiers); cx.stop_propagation(); }
                }))
                .on_mouse_down(MouseButton::Right,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {
                    w.focus(&this.focus,cx);this.linux_pointer("down",event.position,Some(event.button),event.modifiers);cx.stop_propagation();
                }))
                .on_mouse_up(MouseButton::Left,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_up_out(MouseButton::Left,cx.listener(|this,event: &gpui::MouseUpEvent,_,_| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);}))
                .on_mouse_up(MouseButton::Right,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_down(MouseButton::Middle,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {w.focus(&this.focus,cx);this.linux_pointer("down",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_up(MouseButton::Middle,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_move(cx.listener(|this,event: &gpui::MouseMoveEvent,_,cx| {if !cx.has_active_drag(){this.linux_pointer("move",event.position,event.pressed_button,event.modifiers);}}))
                .on_scroll_wheel(cx.listener(|this,event: &gpui::ScrollWheelEvent,_,cx| {
                    if let Some(native)=&this.native {
                        let delta=event.delta.pixel_delta(px(16.));let p=event.position-native.bounds.origin;
                        native.command(serde_json::json!({"cmd":"scroll","x":f32::from(p.x),"y":f32::from(p.y),"dx":-f32::from(delta.x)/40.,"dy":-f32::from(delta.y)/40.,"mods":super::linux::modifiers_mask(event.modifiers)}));
                        cx.stop_propagation();
                    }
                }))
            }
        } else {
            body.child(self.empty_body(&theme, cx))
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let body = body.child(self.empty_body(&theme, cx));

        #[cfg(target_os = "linux")]
        let body = body.when_some(self.linux_menu(&theme, cx), |el, menu| el.child(menu));

        let remote_loopback = self.remote
            && self
                .page
                .url
                .as_deref()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| {
                    super::model::loopback(&u)
                        && !(u.port() == Some(zeron_proto::PREVIEW_PROXY_PORT)
                            && u.host_str()
                                .is_some_and(|host| host.ends_with(".localhost")))
                });
        div().id("browser-surface").size_full().flex().flex_col().track_focus(&self.focus)
            .key_context("Browser").on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(|this,event: &gpui::KeyUpEvent,w,cx| {
                #[cfg(target_os = "linux")]
                if this.focus.is_focused(w) {this.linux_key(&event.keystroke,false);cx.stop_propagation();}
                #[cfg(not(target_os = "linux"))]
                let _=(this,event,w,cx);
            }))
            .on_action(cx.listener(|this, _: &super::Reload, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &super::FocusAddress, w, cx| this.focus_address(w, cx)))
            .on_action(cx.listener(|_, _: &super::NewTab, _, cx| cx.emit(BrowserEvent::NewTab(None))))
            .on_action(cx.listener(|_, _: &super::CloseTab, _, cx| cx.emit(BrowserEvent::Close)))
            .on_action(cx.listener(|this, _: &super::Back, _, _| this.history(false)))
            .on_action(cx.listener(|this, _: &super::Forward, _, _| this.history(true)))
            .child(toolbar)
            .when_some(self.validation.clone(), |el, message| el.child(div().px(px(12.0)).py(px(8.0)).text_size(crate::typography::ui_rems(11.0)).text_color(theme.danger).child(message)))
            .when(remote_loopback, |el| el.child(div().px(px(12.0)).py(px(8.0)).border_b_1().border_color(theme.border)
                .text_size(crate::typography::ui_rems(11.0)).text_color(theme.text_muted)
                .child("Localhost opens on this device. Open a detected preview from a new tab to reach your other device.")))
            .child(body)
            .when(external, |el| el.child(div().h(px(26.0)).px(px(10.0)).flex().items_center().gap(px(5.0)).border_t_1().border_color(theme.border)
                .text_size(crate::typography::ui_rems(10.0)).text_color(theme.text_faint)
                .child(icons::icon(icons::ARROW_UP_RIGHT).size(px(11.0))).child("Opens in your default browser")))
    }
}
