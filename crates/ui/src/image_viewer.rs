//! Shared image geometry and native pointer gestures. Scaling reuses the texture.
use gpui::{
    AnyElement, App, Bounds, Image, MouseButton, Pixels, Point, ScrollDelta, Size, TouchPhase,
    Window, div, point, prelude::*, px, size,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct ImageView(Rc<RefCell<ViewState>>);

#[derive(Default)]
struct ViewState {
    owner: Option<gpui::EntityId>,
    geometry: Geometry,
    bounds: Bounds<Pixels>,
    drag: Option<(Point<f32>, Point<f32>)>,
    dragged: bool,
    pinch: Option<(f32, f32)>,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    natural: Size<f32>,
    viewport: Size<f32>,
    scale: f32,
    pan: Point<f32>,
    fitted: bool,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            natural: size(1.0, 1.0),
            viewport: size(1.0, 1.0),
            scale: 1.0,
            pan: point(0.0, 0.0),
            fitted: true,
        }
    }
}

impl Geometry {
    fn fit_scale(&self) -> f32 {
        (self.viewport.width / self.natural.width)
            .min(self.viewport.height / self.natural.height)
            .min(1.0)
    }
    fn resize(&mut self, natural: Size<f32>, viewport: Size<f32>) {
        if ![
            natural.width,
            natural.height,
            viewport.width,
            viewport.height,
        ]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
        {
            return;
        }
        self.natural = natural;
        self.viewport = viewport;
        if self.fitted {
            self.fit();
        } else {
            self.clamp_pan();
        }
    }
    fn fit(&mut self) {
        self.scale = self.fit_scale();
        self.pan = point(0.0, 0.0);
        self.fitted = true;
    }
    fn clamp_pan(&mut self) {
        let limit = point(
            ((self.natural.width * self.scale - self.viewport.width) / 2.0).max(0.0),
            ((self.natural.height * self.scale - self.viewport.height) / 2.0).max(0.0),
        );
        self.pan.x = self.pan.x.clamp(-limit.x, limit.x);
        self.pan.y = self.pan.y.clamp(-limit.y, limit.y);
    }
    fn zoom(&mut self, scale: f32, anchor: Point<f32>) {
        if !scale.is_finite() || scale <= 0.0 || !anchor.x.is_finite() || !anchor.y.is_finite() {
            return;
        }
        // Bound layout coordinates even for SVGs with enormous logical dimensions.
        let maximum = (131072.0 / self.natural.width.max(self.natural.height))
            .min(32.0)
            .max(self.fit_scale());
        let scale = scale.clamp(self.fit_scale().min(0.01), maximum);
        let ratio = scale / self.scale;
        let anchor = anchor - point(self.viewport.width / 2.0, self.viewport.height / 2.0);
        self.pan = point(
            anchor.x - (anchor.x - self.pan.x) * ratio,
            anchor.y - (anchor.y - self.pan.y) * ratio,
        );
        self.scale = scale;
        self.fitted = false;
        self.clamp_pan();
    }
    fn pan_by(&mut self, delta: Point<f32>) -> bool {
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return false;
        }
        let before = self.pan;
        self.pan = self.pan + delta;
        self.clamp_pan();
        self.pan != before
    }
    fn image_origin(&self) -> Point<f32> {
        point(
            (self.viewport.width - self.natural.width * self.scale) / 2.0 + self.pan.x,
            (self.viewport.height - self.natural.height * self.scale) / 2.0 + self.pan.y,
        )
    }
}

fn scroll_pixels(delta: ScrollDelta) -> Point<f32> {
    match delta {
        ScrollDelta::Pixels(p) => point(f32::from(p.x), f32::from(p.y)),
        ScrollDelta::Lines(p) => point(p.x * 40.0, p.y * 40.0),
    }
}

impl ViewState {
    fn local(&self, position: Point<Pixels>) -> Point<f32> {
        point(
            f32::from(position.x - self.bounds.origin.x),
            f32::from(position.y - self.bounds.origin.y),
        )
    }
    fn pinch(&mut self, event: &gpui::PinchEvent) {
        if event.phase == TouchPhase::Ended {
            self.pinch = None;
            return;
        }
        if !event.delta.is_finite() {
            return;
        }
        if event.phase == TouchPhase::Started {
            self.pinch = Some((self.geometry.scale, 1.0));
        }
        let (start, factor) = self.pinch.get_or_insert((self.geometry.scale, 1.0));
        *factor = (*factor + event.delta).max(0.001);
        let scale = *start * *factor;
        let local = self.local(event.position);
        self.geometry.zoom(scale, local);
    }
    fn wheel(&mut self, event: &gpui::ScrollWheelEvent) -> bool {
        let delta = scroll_pixels(event.delta);
        if event.modifiers.control {
            let scale = self.geometry.scale * (delta.y * 0.0025).clamp(-2.0, 2.0).exp();
            self.geometry.zoom(scale, self.local(event.position));
            true
        } else {
            self.geometry.pan_by(delta)
        }
    }
    fn pointer_down(&mut self, position: Point<Pixels>) {
        self.dragged = false;
        self.drag = Some((self.local(position), self.geometry.pan));
    }
    fn pointer_move(&mut self, event: &gpui::MouseMoveEvent) -> bool {
        if !event.dragging() {
            self.drag = None;
            return false;
        }
        let Some((start, pan)) = self.drag else {
            return false;
        };
        let delta = self.local(event.position) - start;
        if delta.x.hypot(delta.y) >= 4.0 {
            self.dragged = true;
        }
        if !self.dragged {
            return false;
        }
        self.geometry.pan = pan;
        self.geometry.pan_by(delta);
        true
    }
}

pub(crate) type ImageClick = Rc<dyn Fn(&mut Window, &mut App)>;

impl ImageView {
    pub fn begin_click(&self) {
        let mut state = self.0.borrow_mut();
        state.drag = None;
        state.dragged = false;
    }

    #[cfg(test)]
    pub fn test_scale(&self) -> f32 {
        self.0.borrow().geometry.scale
    }

    pub fn dragged(&self) -> bool {
        self.0.borrow().dragged
    }

    pub fn reset(&self) {
        *self.0.borrow_mut() = ViewState::default();
    }

    pub fn render(
        &self,
        image: Arc<Image>,
        natural: Size<Pixels>,
        on_image_click: Option<ImageClick>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        let natural = size(f32::from(natural.width), f32::from(natural.height));
        {
            let mut state = self.0.borrow_mut();
            let bounds = state.bounds;
            state.geometry.resize(
                natural,
                size(f32::from(bounds.size.width), f32::from(bounds.size.height)),
            );
        }
        let geometry = self.0.borrow().geometry;
        let origin = geometry.image_origin();
        let measure = self.clone();
        let wheel = self.clone();
        let pinch = self.clone();
        let down = self.clone();
        let movement = self.clone();
        let up = self.clone();
        let up_out = self.clone();
        let click = self.clone();
        let viewport = div()
            .id("image-viewport")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .w_full()
            .relative()
            .overflow_hidden()
            .on_scroll_wheel(move |event, window, cx| {
                if wheel.0.borrow_mut().wheel(event) {
                    cx.stop_propagation();
                    window.prevent_default();
                    if let Some(owner) = wheel.0.borrow().owner {
                        cx.notify(owner);
                    }
                }
            })
            .on_pinch(move |event, window, cx| {
                pinch.0.borrow_mut().pinch(event);
                cx.stop_propagation();
                window.prevent_default();
                if let Some(owner) = pinch.0.borrow().owner {
                    cx.notify(owner);
                }
            })
            .on_mouse_down(MouseButton::Left, move |event, window, _| {
                down.0.borrow_mut().pointer_down(event.position);
                window.prevent_default();
            })
            .on_mouse_up(MouseButton::Left, move |_, _, _| {
                up.0.borrow_mut().drag = None;
            })
            .on_mouse_up_out(MouseButton::Left, move |_, _, _| {
                up_out.0.borrow_mut().drag = None;
            })
            .on_click(move |event, window, cx| {
                let state = click.0.borrow();
                if state.dragged {
                    cx.stop_propagation();
                    return;
                }
                let local = state.local(event.position());
                let geometry = state.geometry;
                let origin = geometry.image_origin();
                let inside = local.x >= origin.x
                    && local.y >= origin.y
                    && local.x <= origin.x + geometry.natural.width * geometry.scale
                    && local.y <= origin.y + geometry.natural.height * geometry.scale;
                drop(state);
                if inside && let Some(on_click) = &on_image_click {
                    cx.stop_propagation();
                    on_click(window, cx);
                }
            })
            .child(
                gpui::img(image)
                    .absolute()
                    .left(px(origin.x))
                    .top(px(origin.y))
                    .w(px(natural.width * geometry.scale))
                    .h(px(natural.height * geometry.scale))
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        let mut state = measure.0.borrow_mut();
                        // Capture during prepaint, when GPUI knows the owning view.
                        let owner = window.current_view();
                        state.owner = Some(owner);
                        if state.bounds != bounds {
                            state.bounds = bounds;
                            state.geometry.resize(
                                natural,
                                size(f32::from(bounds.size.width), f32::from(bounds.size.height)),
                            );
                            window.defer(cx, move |_, cx| cx.notify(owner));
                        }
                    },
                    move |_, _, window, _| {
                        // Continue an image drag beyond its viewport without taking over
                        // other pointers. Release is handled inside and outside the hitbox.
                        let movement = movement.clone();
                        window.on_mouse_event(
                            move |event: &gpui::MouseMoveEvent, phase, _window, cx| {
                                if phase == gpui::DispatchPhase::Bubble
                                    && movement.0.borrow_mut().pointer_move(event)
                                {
                                    cx.stop_propagation();
                                    if let Some(owner) = movement.0.borrow().owner {
                                        cx.notify(owner);
                                    }
                                }
                            },
                        );
                    },
                )
                .absolute()
                .inset_0(),
            );
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(viewport)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn geometry() -> Geometry {
        let mut g = Geometry::default();
        g.resize(size(1000.0, 500.0), size(500.0, 300.0));
        g
    }
    #[test]
    fn fit_preserves_aspect_ratio_and_never_upscales() {
        let mut g = geometry();
        assert_eq!(g.scale, 0.5);
        assert_eq!(g.image_origin(), point(0.0, 25.0));
        g.resize(size(100.0, 50.0), size(500.0, 300.0));
        assert_eq!(g.scale, 1.0);
        assert_eq!(g.image_origin(), point(200.0, 125.0));
    }
    #[test]
    fn zoom_keeps_the_cursor_over_the_same_image_point() {
        let mut g = geometry();
        let anchor = point(100.0, 150.0);
        let before = (anchor - g.image_origin()) / g.scale;
        g.zoom(2.0, anchor);
        let after = (anchor - g.image_origin()) / g.scale;
        assert!((before.x - after.x).abs() < 0.001);
        assert!((before.y - after.y).abs() < 0.001);
    }
    #[test]
    fn pan_zoom_and_resize_stay_bounded() {
        let mut g = geometry();
        g.zoom(1e9, point(250.0, 150.0));
        assert_eq!(g.scale, 32.0);
        g.pan_by(point(1e9, -1e9));
        assert_eq!(g.pan, point(15750.0, -7850.0));
        g.zoom(1e-9, point(250.0, 150.0));
        assert_eq!(g.scale, 0.01);
        assert_eq!(g.pan, point(0.0, 0.0));
        g.zoom(f32::NAN, point(0.0, 0.0));
        assert_eq!(g.scale, 0.01);
        g.fit();
        g.resize(size(1000.0, 500.0), size(250.0, 200.0));
        assert_eq!(g.scale, 0.25);
        g.zoom(1.0, point(125.0, 100.0));
        g.resize(size(1000.0, 500.0), size(300.0, 200.0));
        assert_eq!(g.scale, 1.0);
    }
    #[test]
    fn wheel_requires_control_and_normalizes_lines() {
        let mut state = ViewState {
            geometry: geometry(),
            ..Default::default()
        };
        let mut event = gpui::ScrollWheelEvent {
            position: point(px(250.0), px(150.0)),
            delta: ScrollDelta::Lines(point(0.0, 1.0)),
            ..Default::default()
        };
        assert!(!state.wheel(&event));
        assert_eq!(state.geometry.scale, 0.5);
        event.modifiers.control = true;
        assert!(state.wheel(&event));
        let scale = state.geometry.scale;
        assert!(scale > 0.5, "GPUI positive Y (wheel up) must zoom in");
        state.geometry.fit();
        event.delta = ScrollDelta::Pixels(point(px(0.0), px(40.0)));
        state.wheel(&event);
        assert_eq!(state.geometry.scale, scale);
        event.delta = ScrollDelta::Lines(point(0.0, -1.0));
        state.wheel(&event);
        assert!(
            (state.geometry.scale - 0.5).abs() < 0.0001,
            "wheel down reverses zoom"
        );
    }
    #[test]
    fn pinch_accumulates_native_deltas_and_a_drag_does_not_click() {
        let mut state = ViewState {
            geometry: geometry(),
            ..Default::default()
        };
        let mut event = gpui::PinchEvent {
            position: point(px(250.0), px(150.0)),
            phase: TouchPhase::Started,
            ..Default::default()
        };
        state.pinch(&event);
        event.phase = TouchPhase::Moved;
        event.delta = 0.2;
        state.pinch(&event);
        state.pinch(&event);
        assert!((state.geometry.scale - 0.7).abs() < 0.001);
        event.phase = TouchPhase::Ended;
        state.pinch(&event);
        assert!(state.pinch.is_none());
        state.pointer_down(point(px(200.0), px(150.0)));
        assert!(!state.pointer_move(&gpui::MouseMoveEvent {
            position: point(px(202.0), px(150.0)),
            pressed_button: Some(MouseButton::Left),
            ..Default::default()
        }));
        assert!(state.pointer_move(&gpui::MouseMoveEvent {
            position: point(px(220.0), px(150.0)),
            pressed_button: Some(MouseButton::Left),
            ..Default::default()
        }));
        assert!(state.dragged);
        state.pointer_down(point(px(200.0), px(150.0)));
        assert!(!state.dragged);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn rendered_lightbox_consumes_zoom_and_drag_but_allows_click_and_escape() {
        use gpui::{AppContext, Context, Render};
        struct CachedSibling(Rc<std::cell::Cell<usize>>);
        impl Render for CachedSibling {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.set(self.0.get() + 1);
                div().size_full()
            }
        }
        struct Harness {
            sibling: gpui::Entity<CachedSibling>,
            preview: crate::attachments::PreviewImage,
            focus: gpui::FocusHandle,
            closed: bool,
        }
        impl Render for Harness {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                if self.closed {
                    return div().into_any_element();
                }
                let weak = cx.weak_entity();
                div()
                    .size_full()
                    .child(
                        self.sibling
                            .clone()
                            .cached(gpui::StyleRefinement::default()),
                    )
                    .child(crate::attachments::lightbox_with_size(
                        window,
                        &self.preview,
                        &self.focus,
                        Some(size(px(1000.0), px(500.0))),
                        move |_, cx| {
                            weak.update(cx, |view, cx| {
                                view.closed = true;
                                cx.notify();
                            })
                            .unwrap();
                        },
                        cx,
                    ))
                    .into_any_element()
            }
        }
        gpui_platform::headless().run(|cx| {
            cx.set_global(crate::theme::Theme::dark());
            let window = cx.open_window(gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(Point::default(), size(px(600.0), px(400.0))))),
                ..Default::default()
            }, |window, cx| cx.new(|cx| {
                let media = crate::image_media::decode_image("image/svg+xml", br#"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="500"><rect width="1000" height="500"/></svg>"#.to_vec()).unwrap();
                let focus = cx.focus_handle(); window.focus(&focus, cx);
                Harness { sibling: cx.new(|_| CachedSibling(Rc::new(std::cell::Cell::new(0)))), preview: crate::attachments::PreviewImage::new("test.svg", media.image), focus, closed: false }
            })).unwrap();
            let harness = window.entity(cx).unwrap();
            for _ in 0..3 { cx.update_window(window.into(), |_, window, cx| { window.refresh(); let _ = window.draw(cx); }).unwrap(); }
            let viewer = harness.read(cx).preview.viewer.clone();
            let bounds = viewer.0.borrow().bounds;
            assert!(bounds.size.width > px(100.0));
            let position = bounds.center();
            let initial = viewer.0.borrow().geometry.scale;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position, ..Default::default() }), cx);
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position, delta: ScrollDelta::Pixels(point(px(0.0), px(200.0))), modifiers: gpui::Modifiers { control: true, ..Default::default() }, ..Default::default()
                }), cx);
            }).unwrap();
            assert!(viewer.0.borrow().geometry.scale > initial);
            assert!(!harness.read(cx).closed);
            let sibling_renders = harness.read(cx).sibling.read(cx).0.clone();
            let before_draw = sibling_renders.get();
            assert!(before_draw > 0);
            cx.update_window(window.into(), |_, window, cx| { let _ = window.draw(cx); }).unwrap();
            assert_eq!(sibling_renders.get(), before_draw, "zoom must preserve unrelated cached views");
            let after_wheel = viewer.0.borrow().geometry.scale;
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh(); let _ = window.draw(cx);
                for (phase, delta) in [(TouchPhase::Started, 0.0), (TouchPhase::Moved, 0.5), (TouchPhase::Ended, 0.0)] {
                    window.dispatch_event(gpui::PlatformInput::Pinch(gpui::PinchEvent { position, phase, delta, ..Default::default() }), cx);
                }
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { position, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
                let moved = position + point(px(50.0), px(0.0));
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: moved, pressed_button: Some(MouseButton::Left), ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { position: moved, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
            }).unwrap();
            assert!((viewer.0.borrow().geometry.scale - after_wheel * 1.5).abs() < 0.001);
            assert!(!harness.read(cx).closed, "drag must not close the lightbox");
            assert_ne!(viewer.0.borrow().geometry.pan, point(0.0, 0.0));
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position, ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { position, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
                let outside = point(bounds.right() + px(5.0), position.y);
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: outside, pressed_button: Some(MouseButton::Left), ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { position: outside, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
            }).unwrap();
            assert!(!harness.read(cx).closed, "a drag ending on the scrim must not close");
            let position = point(px(5.0), px(5.0));

            cx.update_window(window.into(), |_, window, cx| {
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position, ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { position, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
                window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { position, button: MouseButton::Left, click_count: 1, ..Default::default() }), cx);
            }).unwrap();
            assert!(harness.read(cx).closed, "a plain click closes");
            harness.update(cx, |view, cx| { view.closed = false; view.preview.viewer.reset(); cx.notify(); });
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh(); let _ = window.draw(cx);
                window.dispatch_event(gpui::PlatformInput::KeyDown(gpui::KeyDownEvent { keystroke: gpui::Keystroke::parse("escape").unwrap(), is_held: false, prefer_character_input: false }), cx);
            }).unwrap();
            assert!(harness.read(cx).closed);
            cx.spawn(async move |cx| { cx.update(|cx| cx.quit()); }).detach();
        });
    }
}
