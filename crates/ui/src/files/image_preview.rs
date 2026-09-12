//! A read-only workspace image. The client resolves bytes on the owning device.
use super::client::{FilesRequestContext, WorkspaceFilesClient};
use crate::{
    image_media::{MediaImage, decode_image, release_media},
    theme::Theme,
};
use gpui::{Bounds, Context, FocusHandle, Pixels, Render, Task, Window, div, prelude::*, px};
use std::{sync::Arc, time::Duration};

const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn is_image(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff"
        )
    })
}

pub(super) struct ImagePreview {
    pub focus: FocusHandle,
    path: String,
    context: FilesRequestContext,
    client: WorkspaceFilesClient,
    generation: u64,
    task: Option<Task<()>>,
    source: Option<MediaImage>,
    display: Option<MediaImage>,
    error: Option<String>,
    suspended: bool,
    bounds: Bounds<Pixels>,
    viewer: crate::image_viewer::ImageView,
}

impl ImagePreview {
    pub fn new(
        path: String,
        context: FilesRequestContext,
        client: WorkspaceFilesClient,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.on_release(|view, cx| view.release(cx)).detach();
        let mut view = Self {
            focus: cx.focus_handle(),
            path,
            context,
            client,
            generation: 0,
            task: None,
            source: None,
            display: None,
            error: None,
            suspended: false,
            bounds: Bounds::default(),
            viewer: Default::default(),
        };
        view.reload(cx);
        view
    }

    fn release(&mut self, cx: &mut gpui::App) {
        self.viewer.reset();
        release_media(
            self.source.take().into_iter().chain(self.display.take()),
            cx,
        );
    }

    pub fn suspend(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.task = None;
        self.suspended = true;
        self.release(cx);
        cx.notify();
    }

    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if self.suspended {
            self.reload(cx);
        }
    }

    pub fn deleted(&mut self, cx: &mut Context<Self>) {
        self.suspend(cx);
        self.suspended = false;
        self.error = Some("This image was removed from the workspace.".into());
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.suspend(cx);
        self.suspended = false;
        self.error = None;
        let generation = self.generation;
        let path = self.path.clone();
        let context = self.context.clone();
        let client = self.client.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let load = async {
                // Legacy chats and plain folders may not carry a checkout ID in
                // synced metadata. Obtain only its identity from the owning host;
                // no text response is retained or used to render the image.
                let checkout = match context.checkout_id.clone().filter(|id| !id.is_empty()) {
                    Some(id) => id,
                    None => {
                        client
                            .read_file(zeron_proto::ReadWorkspaceFileRequest {
                                target: context.target.clone(),
                                path: path.clone(),
                            })
                            .await
                            .map_err(|e| e.to_string())?
                            .checkout_id
                    }
                };
                if checkout.is_empty() {
                    return Err("Workspace checkout identity unavailable".into());
                }
                let (mime, bytes) = client
                    .read_image(path, checkout)
                    .await
                    .map_err(|e| e.to_string())?;
                executor
                    .spawn(async move { decode_image(&mime, bytes) })
                    .await
            };
            let result = match futures::future::select(
                Box::pin(load),
                Box::pin(executor.timer(Duration::from_secs(30))),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => Err("Image preview timed out".into()),
            };
            let _ = this.update(cx, |view, cx| view.complete(generation, result, cx));
        }));
    }

    fn complete(
        &mut self,
        generation: u64,
        result: Result<MediaImage, String>,
        cx: &mut Context<Self>,
    ) {
        if self.suspended || generation != self.generation {
            return;
        }
        self.task = None;
        match result.and_then(|media| {
            if media.bytes > MAX_MEDIA_BYTES {
                Err("Image exceeds preview memory limit".into())
            } else {
                Ok(media)
            }
        }) {
            Ok(media) => self.source = Some(media),
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

impl Render for ImagePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let mut root = div()
            .id("file-image-preview")
            .track_focus(&self.focus)
            .size_full()
            .min_w_0()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        if let Some(source) = self.source.clone() {
            let viewport = (
                f32::from(self.bounds.size.width).max(1.0),
                f32::from(self.bounds.size.height).max(1.0),
            );
            let display = source.enlarged(
                viewport,
                window.scale_factor(),
                MAX_MEDIA_BYTES.saturating_sub(source.bytes),
                self.display.as_ref(),
            );
            if let Some(old) = self.display.replace(display.clone()) {
                if !Arc::ptr_eq(&old.image, &display.image)
                    && !Arc::ptr_eq(&old.image, &source.image)
                {
                    release_media([old], cx);
                }
            }
            root = root.child(self.viewer.render(
                display.image,
                gpui::size(px(source.width), px(source.height)),
                None,
                window,
                cx,
            ));
        } else {
            root = root.child(
                div()
                    .px(px(16.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(
                        self.error
                            .clone()
                            .unwrap_or_else(|| "Loading image…".into()),
                    ),
            );
        }
        let entity = cx.weak_entity();
        root.child(
            gpui::canvas(
                move |bounds, _, cx| {
                    let _ = entity.update(cx, |view, cx| {
                        if view.bounds != bounds {
                            view.bounds = bounds;
                            cx.notify();
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_supported_workspace_formats() {
        for path in [
            "a.PNG", "a.jpeg", "a.JPG", "a.gif", "a.webp", "a.svg", "a.bmp", "a.tif", "a.tiff",
        ] {
            assert!(is_image(path));
        }
        for path in ["a.rs", "a.md", "a", "a.png.txt", "a.avif"] {
            assert!(!is_image(path));
        }
    }
    struct PendingTransport;
    #[async_trait::async_trait]
    impl super::super::client::WorkspaceFilesTransport for PendingTransport {
        async fn call(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, zeron_rpc::RpcError> {
            std::future::pending().await
        }
        async fn subscribe(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, zeron_rpc::RpcError> {
            unreachable!()
        }
    }
    fn preview(cx: &mut gpui::TestAppContext) -> gpui::Entity<ImagePreview> {
        use gpui::AppContext as _;
        cx.new(|cx| {
            let context = FilesRequestContext {
                target: zeron_proto::WorkspaceTarget {
                    chat_id: Some("remote-chat".into()),
                    space_id: None,
                    checkout_path: None,
                },
                target_device_id: Some("remote-device".into()),
                cwd: "/only/on/remote".into(),
                checkout_id: Some("checkout".into()),
            };
            let client =
                WorkspaceFilesClient::with_transport(Arc::new(PendingTransport), context.clone());
            ImagePreview::new("a.svg".into(), context, client, cx)
        })
    }
    fn media() -> MediaImage {
        decode_image("image/svg+xml", br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50"/></svg>"#.to_vec()).unwrap()
    }
    #[gpui::test]
    fn suspension_cancels_load_rejects_late_results_and_releases_images(
        cx: &mut gpui::TestAppContext,
    ) {
        let view = preview(cx);
        let weak = view.update(cx, |view, cx| {
            let generation = view.generation;
            let loaded = media();
            let weak = Arc::downgrade(&loaded.image);
            view.complete(generation, Ok(loaded), cx);
            assert!(view.source.is_some());
            view.suspend(cx);
            assert!(view.task.is_none());
            assert!(view.source.is_none());
            view.complete(generation, Ok(media()), cx);
            assert!(view.source.is_none());
            view.activate(cx);
            assert!(view.task.is_some());
            view.complete(generation, Err("obsolete failure".into()), cx);
            assert!(view.error.is_none());
            weak
        });
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }
    #[gpui::test]
    fn deletion_and_memory_limits_are_visible_and_release_on_disposal(
        cx: &mut gpui::TestAppContext,
    ) {
        let view = preview(cx);
        view.update(cx, |view, cx| {
            let mut oversized = media();
            oversized.bytes = MAX_MEDIA_BYTES + 1;
            view.complete(view.generation, Ok(oversized), cx);
            assert!(view.error.as_ref().unwrap().contains("memory limit"));
            assert!(view.source.is_none());
            view.reload(cx);
            view.deleted(cx);
            view.activate(cx);
            assert!(view.task.is_none());
            assert!(view.error.as_ref().unwrap().contains("removed"));
        });
        let weak = view.update(cx, |view, cx| {
            view.reload(cx);
            let loaded = media();
            let weak = Arc::downgrade(&loaded.image);
            view.complete(view.generation, Ok(loaded), cx);
            weak
        });
        drop(view);
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }
    #[derive(Default)]
    struct ImageTransport {
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }
    #[async_trait::async_trait]
    impl super::super::client::WorkspaceFilesTransport for ImageTransport {
        async fn call(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, zeron_rpc::RpcError> {
            use base64::Engine as _;
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            assert_eq!(params["targetDeviceId"], "owner");
            assert_eq!(params["chatId"], "chat");
            assert_eq!(params["path"], "remote.svg");
            if method == zeron_rpc::methods::READ_WORKSPACE_FILE {
                return Ok(
                    serde_json::json!({ "checkoutId": "checkout", "path": "remote.svg", "size": 0, "encoding": "binary", "truncated": false }),
                );
            }
            assert_eq!(method, zeron_rpc::methods::READ_WORKSPACE_IMAGE);
            assert_eq!(params["expectedCheckoutId"], "checkout");
            let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="red"/></svg>"#;
            Ok(
                serde_json::json!({ "checkoutId": "checkout", "contentHash": "version", "mimeType": "image/svg+xml", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "nextOffset": bytes.len(), "size": bytes.len(), "done": true }),
            )
        }
        async fn subscribe(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, zeron_rpc::RpcError> {
            unreachable!()
        }
    }
    #[gpui::test]
    fn image_preview_loads_through_owner_and_resolves_legacy_identity(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        for checkout_id in [Some("checkout".to_string()), None] {
            let legacy = checkout_id.is_none();
            let transport = Arc::new(ImageTransport::default());
            let view = cx.new(|cx| {
                let context = FilesRequestContext {
                    target: zeron_proto::WorkspaceTarget {
                        chat_id: Some("chat".into()),
                        space_id: None,
                        checkout_path: None,
                    },
                    target_device_id: Some("owner".into()),
                    cwd: "/not/on/ui/device".into(),
                    checkout_id,
                };
                let client =
                    WorkspaceFilesClient::with_transport(transport.clone(), context.clone());
                ImagePreview::new("remote.svg".into(), context, client, cx)
            });
            cx.run_until_parked();
            view.read_with(cx, |view, _| {
                assert!(view.error.is_none(), "{:?}", view.error);
                let source = view.source.as_ref().expect("remote image loaded");
                assert_eq!((source.width, source.height), (100.0, 50.0));
                assert!(view.task.is_none());
            });
            let calls = transport.calls.lock().unwrap();
            assert_eq!(calls.len(), if legacy { 2 } else { 1 });
            assert_eq!(
                calls.last().unwrap().0,
                zeron_rpc::methods::READ_WORKSPACE_IMAGE
            );
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn rendered_workspace_image_click_keeps_zoom_in_the_file_panel() {
        use gpui::{AppContext, point, size};
        gpui_platform::headless().run(|cx| {
            cx.set_global(Theme::dark());
            let window = cx
                .open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(600.0), px(400.0)),
                        ))),
                        ..Default::default()
                    },
                    |window, cx| {
                        cx.new(|cx| {
                            let context = FilesRequestContext {
                                target: zeron_proto::WorkspaceTarget {
                                    chat_id: Some("chat".into()),
                                    space_id: None,
                                    checkout_path: None,
                                },
                                target_device_id: Some("owner".into()),
                                cwd: "/not/on/ui/device".into(),
                                checkout_id: Some("checkout".into()),
                            };
                            let client = WorkspaceFilesClient::with_transport(
                                Arc::new(ImageTransport::default()),
                                context.clone(),
                            );
                            let view = ImagePreview::new("remote.svg".into(), context, client, cx);
                            window.focus(&view.focus, cx);
                            view
                        })
                    },
                )
                .unwrap();
            let view = window.entity(cx).unwrap();
            cx.spawn(async move |cx| {
                for _ in 0..100 {
                    cx.background_executor()
                        .timer(Duration::from_millis(10))
                        .await;
                    if cx.update(|cx| view.read(cx).source.is_some()) {
                        break;
                    }
                }
                cx.update(|cx| {
                    assert!(view.read(cx).source.is_some(), "owner-routed image loads");
                    for _ in 0..3 {
                        cx.update_window(window.into(), |_, window, cx| {
                            window.refresh();
                            let _ = window.draw(cx);
                        })
                        .unwrap();
                    }
                    assert!(view.read(cx).bounds.size.width > px(100.0));
                    assert_eq!(
                        view.read(cx).viewer.test_scale(),
                        1.0,
                        "small image is not upscaled"
                    );
                    let position = view.read(cx).bounds.center();
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position,
                                ..Default::default()
                            }),
                            cx,
                        );
                        window.refresh();
                        let _ = window.draw(cx);
                        window.dispatch_event(
                            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                                position,
                                button: gpui::MouseButton::Left,
                                click_count: 1,
                                ..Default::default()
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                                position,
                                button: gpui::MouseButton::Left,
                                click_count: 1,
                                ..Default::default()
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    for _ in 0..3 {
                        cx.update_window(window.into(), |_, window, cx| {
                            window.refresh();
                            let _ = window.draw(cx);
                        })
                        .unwrap();
                    }
                    cx.update_window(window.into(), |_, window, cx| {
                        assert!(
                            view.read(cx).focus.is_focused(window),
                            "click keeps focus in the file panel"
                        );
                        window.dispatch_event(
                            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position,
                                ..Default::default()
                            }),
                            cx,
                        );
                        window.refresh();
                        let _ = window.draw(cx);
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position,
                                delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(100.0))),
                                modifiers: gpui::Modifiers {
                                    control: true,
                                    ..Default::default()
                                },
                                ..Default::default()
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    assert!(
                        view.read(cx).viewer.test_scale() > 1.0,
                        "zoom still targets the file panel after clicking the image"
                    );
                    cx.quit();
                });
            })
            .detach();
        });
    }
}
