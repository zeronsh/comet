//! Static native-rendering regression. No engine, credentials, network, or user data.
//! Run through scripts/test-windows-rendering.ps1 on an interactive Windows desktop.
use gpui::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, TitlebarOptions,
    Window, WindowBounds, WindowOptions, div, img, point, px, rgb, size,
};
use std::path::PathBuf;
use zeron_ui::edge_fade::edge_faded;

struct Fixture {
    image: PathBuf,
}

impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .bg(rgb(0x101820))
            .text_color(rgb(0xffffff))
            .font_family("Segoe UI")
            .text_size(px(18.))
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(20.))
                    .child("Windows GPU fixture / Text 012345"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(60.))
                    .child("Quad: left/right fades"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(360.))
                    .top(px(60.))
                    .child("Quad: reference"),
            )
            .child(
                div().absolute().left(px(40.)).top(px(90.)).child(
                    edge_faded(
                        40.,
                        false,
                        false,
                        div().w(px(240.)).h(px(80.)).bg(rgb(0xff4020)),
                    )
                    .fade_left(true)
                    .fade_right(true),
                ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(360.))
                    .top(px(90.))
                    .w(px(240.))
                    .h(px(80.))
                    .bg(rgb(0xff4020)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(195.))
                    .child("PNG atlas: reference"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(360.))
                    .top(px(195.))
                    .child("PNG atlas: fades"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(225.))
                    .child(img(self.image.clone()).w(px(240.)).h(px(120.))),
            )
            .child(
                div().absolute().left(px(360.)).top(px(225.)).child(
                    edge_faded(
                        40.,
                        false,
                        false,
                        img(self.image.clone()).w(px(240.)).h(px(120.)),
                    )
                    .fade_left(true)
                    .fade_right(true),
                ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(385.))
                    .w(px(560.))
                    .h(px(60.))
                    .bg(rgb(0x223344))
                    .border_1()
                    .border_color(rgb(0x6b879f))
                    .rounded(px(8.))
                    .p(px(16.))
                    .child("Raised panel / opaque fallback / no blur claim"),
            )
            .child(
                div()
                    .absolute()
                    .left(px(40.))
                    .top(px(455.))
                    .child("Vertical fades: top 20 / bottom 40"),
            )
            .child(
                div().absolute().left(px(40.)).top(px(485.)).child(
                    edge_faded(
                        40.,
                        true,
                        true,
                        div().w(px(240.)).h(px(120.)).bg(rgb(0x2080f0)),
                    )
                    .band_top(20.)
                    .band_bottom(40.),
                ),
            )
            .child(
                div().absolute().left(px(360.)).top(px(485.)).child(
                    edge_faded(
                        40.,
                        true,
                        true,
                        img(self.image.clone()).w(px(240.)).h(px(120.)),
                    )
                    .band_top(20.)
                    .band_bottom(40.),
                ),
            )
    }
}

fn main() -> anyhow::Result<()> {
    let image = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("expected a local PNG path"))?,
    );
    anyhow::ensure!(image.is_file(), "fixture PNG does not exist");
    tracing_subscriber::fmt().with_env_filter("info").init();
    gpui_platform::application().run(move |cx| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(40.), px(40.)),
                    size(px(680.), px(640.)),
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Zeron Windows rendering fixture".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Fixture { image }),
        )
        .expect("open rendering fixture");
        cx.activate(true);
    });
    Ok(())
}
