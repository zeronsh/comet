//! Bounded media decoding. SVG resources are resolved in memory, never on the UI host.
use gpui::{Image, ImageFormat};
use std::{
    io::Cursor,
    sync::{Arc, OnceLock},
};

pub(crate) fn release_media(images: impl IntoIterator<Item = MediaImage>, cx: &mut gpui::App) {
    let images: Vec<_> = images.into_iter().map(|media| media.image).collect();
    // After the active window returns to App, release its atlas tiles as well.
    cx.defer(move |cx| {
        for image in images {
            gpui::ImageSource::Image(image).evict(None, cx);
        }
    });
}

#[derive(Clone)]
pub(crate) struct MediaImage {
    pub image: Arc<Image>,
    pub width: f32,
    pub height: f32,
    pub bytes: usize,
    svg: Option<Arc<str>>,
    raster_size: Option<(u32, u32)>,
}

const PREVIEW_PIXELS: usize = 1024 * 1024;
const MAX_RASTER_SIDE: f64 = 4096.0;
// GPUI's SvgRenderer rasterizes SVG images at twice their declared dimensions.
const GPUI_SVG_SCALE: f64 = 2.0;

fn raster_size(
    width: f32,
    height: f32,
    viewport: (f32, f32),
    dpi: f32,
    pixels: usize,
) -> (u32, u32) {
    let (w, h) = (width as f64, height as f64);
    let dpi = f64::from(dpi.clamp(1.0, 4.0));
    let fit = (f64::from(viewport.0.max(1.0)) / w)
        .min(f64::from(viewport.1.max(1.0)) / h)
        .min(1.0);
    let scale = (fit * dpi)
        .min(MAX_RASTER_SIDE / w)
        .min(MAX_RASTER_SIDE / h)
        .min((pixels.max(1) as f64 / (w * h)).sqrt());
    let mut size = (
        (w * scale).floor().max(1.0) as u32,
        (h * scale).floor().max(1.0) as u32,
    );
    if size.0 as usize * size.1 as usize > pixels.max(1) {
        if size.0 > size.1 {
            size.0 = (pixels.max(1) / size.1 as usize).max(1) as u32;
        } else {
            size.1 = (pixels.max(1) / size.0 as usize).max(1) as u32;
        }
    }
    size
}

impl MediaImage {
    /// Preserve the sanitized vector source; only the outer raster viewport changes.
    pub(crate) fn for_view(&self, viewport: (f32, f32), dpi: f32, pixels: usize) -> Self {
        let Some(svg) = &self.svg else {
            return self.clone();
        };
        let size = raster_size(self.width, self.height, viewport, dpi, pixels);
        if self.raster_size == Some(size) {
            return self.clone();
        }
        let wrapper = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}">{}</svg>"#,
            size.0 as f64 / GPUI_SVG_SCALE,
            size.1 as f64 / GPUI_SVG_SCALE,
            self.width,
            self.height,
            svg
        );
        Self {
            image: Arc::new(Image::from_bytes(ImageFormat::Svg, wrapper.into_bytes())),
            width: self.width,
            height: self.height,
            bytes: self
                .bytes
                .max(svg.len() * 2 + 1024 + size.0 as usize * size.1 as usize * 8),
            svg: self.svg.clone(),
            raster_size: Some(size),
        }
    }

    pub(crate) fn preview_for_view(&self, viewport: (f32, f32), dpi: f32) -> Self {
        self.for_view(viewport, dpi, PREVIEW_PIXELS)
    }

    pub(crate) fn enlarged(
        &self,
        viewport: (f32, f32),
        dpi: f32,
        available: usize,
        cached: Option<&Self>,
    ) -> Self {
        let Some(svg) = &self.svg else {
            return self.clone();
        };
        let pixels = available.saturating_sub(svg.len() * 2 + 1024) / 8;
        if pixels < self.raster_size.map_or(0, |(w, h)| w as usize * h as usize) {
            return self.clone();
        }
        let pixels = pixels.min(2 * PREVIEW_PIXELS);
        let size = raster_size(self.width, self.height, viewport, dpi, pixels);
        if let Some(cached) = cached.filter(|cached| cached.raster_size == Some(size)) {
            return cached.clone();
        }
        self.for_view(viewport, dpi, pixels)
    }
}

pub(crate) fn svg_options() -> usvg::Options<'static> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            for face in crate::typography::bundled_font_faces() {
                db.load_font_data(face.to_vec());
            }
            db.set_sans_serif_family("Geist");
            db.set_monospace_family("Geist Mono");
            // usvg appends generic serif when a requested family is unavailable
            // (including GPUI's virtual .SystemUIFont). Its default may refer to
            // an uninstalled font, silently deleting text during outlining.
            db.set_serif_family("Geist");
            Arc::new(db)
        })
        .clone();
    usvg::Options {
        fontdb: fonts,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

pub(crate) fn decode_image(mime: &str, bytes: Vec<u8>) -> Result<MediaImage, String> {
    if bytes.len() > zeron_proto::MAX_WORKSPACE_IMAGE_BYTES {
        return Err("Image exceeds preview size limit".into());
    }
    if mime == "image/svg+xml" {
        let tree = usvg::Tree::from_data(&bytes, &svg_options()).map_err(|e| e.to_string())?;
        let (width, height) = (tree.size().width(), tree.size().height());
        // Re-serialize the parsed tree: scripts, HTML and external resources never reach GPUI.
        let svg = tree.to_string(&usvg::WriteOptions::default());
        if svg.len() > zeron_proto::MAX_WORKSPACE_IMAGE_BYTES {
            return Err("Prepared SVG exceeds preview size limit".into());
        }
        let maximum = raster_size(width, height, (900.0, 480.0), 4.0, PREVIEW_PIXELS);
        // Reserve the largest admitted preview across supported display densities,
        // including both CPU pixels and GPU texture, plus source and wrapper.
        let retained = svg.len() * 2 + 1024 + maximum.0 as usize * maximum.1 as usize * 8;
        let media = MediaImage {
            image: Arc::new(Image::from_bytes(ImageFormat::Svg, Vec::new())),
            width,
            height,
            bytes: retained,
            svg: Some(Arc::from(svg)),
            raster_size: None,
        };
        return Ok(media.preview_for_view((900.0, 480.0), 2.0));
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    // Decode one frame and encode a static PNG so GPUI cannot expand unbounded animation frames.
    let decoded = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (decoded.width() as f32, decoded.height() as f32);
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let bytes = png.into_inner();
    // GPUI retains decoded CPU pixels as well as the uploaded GPU texture.
    let retained = bytes.len() + decoded.width() as usize * decoded.height() as usize * 8;
    Ok(MediaImage {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
        width,
        height,
        bytes: retained,
        svg: None,
        raster_size: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_svg_text_is_visible(family: &str) {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="80"><text x="10" y="45" font-family="{family}" font-size="24" fill="black">Modelo de datos</text></svg>"#
        );
        let media = decode_image("image/svg+xml", svg.into_bytes()).unwrap();
        let raster = media
            .image
            .to_image_data(gpui::SvgRenderer::new(Arc::new(crate::icons::Assets)))
            .unwrap();
        let visible_pixels = raster
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 128)
            .count();
        assert!(
            visible_pixels > 100,
            "{family}: text disappeared ({visible_pixels} visible pixels)"
        );
    }

    #[test]
    fn svg_text_survives_bundled_mono_font_selection() {
        assert_svg_text_is_visible("Geist Mono");
    }

    #[test]
    fn svg_text_survives_an_unavailable_font() {
        assert_svg_text_is_visible("Zeron Missing SVG Test Font");
        assert_svg_text_is_visible(".SystemUIFont");
    }

    #[test]
    fn svg_is_bounded_and_external_resources_are_removed() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><image href="file:///etc/passwd" width="20" height="20"/><rect width="10" height="10"/></svg>"##;
        let media = decode_image("image/svg+xml", svg.to_vec()).unwrap();
        assert!(!media.svg.unwrap().contains("file:///etc/passwd"));
        assert!(
            decode_image(
                "image/svg+xml",
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="99999" height="10"/>"#.to_vec()
            )
            .is_ok()
        );
        assert!(decode_image("image/png", b"not an image".to_vec()).is_err());
    }

    #[test]
    fn large_svgs_keep_the_complete_viewbox_at_bounded_resolutions() {
        for (width, height) in [(40000, 500), (500, 40000), (40000, 40000)] {
            let source = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="100 200 {width} {height}"><rect x="100" y="200" width="{width}" height="{height}" fill="red"/><rect x="{}" y="{}" width="{}" height="{}" fill="blue"/></svg>"#,
                100 + width / 2,
                200 + height / 2,
                width / 2,
                height / 2
            );
            let media = decode_image("image/svg+xml", source.into_bytes()).unwrap();
            assert_eq!((media.width, media.height), (width as f32, height as f32));
            for dpi in [1.0, 2.0, 4.0] {
                let thumbnail = media.preview_for_view((600.0, 480.0), dpi);
                let enlarged = media.enlarged((1600.0, 1000.0), dpi, 32 * 1024 * 1024, None);
                for (variant, limit) in [
                    (&thumbnail, PREVIEW_PIXELS),
                    (&enlarged, 2 * PREVIEW_PIXELS),
                ] {
                    let (w, h) = variant.raster_size.unwrap();
                    assert!(w <= 4096 && h <= 4096 && w as usize * h as usize <= limit);
                    assert!(
                        variant.bytes
                            >= w as usize * h as usize * 8
                                + variant.svg.as_ref().unwrap().len() * 2
                    );
                    let raster = variant
                        .image
                        .to_image_data(gpui::SvgRenderer::new(Arc::new(crate::icons::Assets)))
                        .unwrap();
                    let bytes = raster.as_bytes(0).unwrap();
                    assert_eq!(bytes.len(), w as usize * h as usize * 4);
                    let pixel = |x: u32, y: u32| {
                        &bytes[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize]
                    };
                    // GPUI returns BGRA. Opposite quadrants must both survive scaling.
                    assert!(pixel(w / 4, h / 4)[2] > 200);
                    assert!(pixel(w * 3 / 4, h * 3 / 4)[0] > 200);
                }
                let cached =
                    media.enlarged((1600.0, 1000.0), dpi, 32 * 1024 * 1024, Some(&enlarged));
                assert!(Arc::ptr_eq(&cached.image, &enlarged.image));
                let fallback = media.enlarged((1600.0, 1000.0), dpi, 0, Some(&enlarged));
                assert!(Arc::ptr_eq(&fallback.image, &media.image));
            }
        }
    }

    #[test]
    fn extreme_aspect_ratios_stay_inside_the_pixel_budget() {
        for (w, h) in [(1e20, 1.0), (1.0, 1e20)] {
            let size = raster_size(w, h, (2000.0, 2000.0), 4.0, 16);
            assert!(size.0 as usize * size.1 as usize <= 16);
        }
    }
    #[test]
    fn encoded_input_and_decode_dimensions_are_bounded() {
        assert!(
            decode_image(
                "image/png",
                vec![0; zeron_proto::MAX_WORKSPACE_IMAGE_BYTES + 1]
            )
            .is_err()
        );
        for (width, height) in [(4097, 1), (1, 4097)] {
            let mut png = Cursor::new(Vec::new());
            image::RgbaImage::new(width, height)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            assert!(decode_image("image/png", png.into_inner()).is_err());
        }
    }

    #[test]
    fn svg_scripts_html_and_embedded_or_external_images_never_reach_gpui() {
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
          <script>alert('unsafe-script')</script>
          <foreignObject width="50" height="50"><div xmlns="http://www.w3.org/1999/xhtml">unsafe-html</div></foreignObject>
          <image href="https://example.invalid/unsafe-network.png" width="10" height="10"/>
          <image href="file:///unsafe-local.png" width="10" height="10"/>
          <image href="data:image/svg+xml;base64,PHN2Zy8+" width="10" height="10"/>
          <rect width="100" height="50" fill="red"/>
        </svg>"#;
        let media = decode_image("image/svg+xml", source.to_vec()).unwrap();
        let sanitized = media.svg.unwrap();
        for forbidden in [
            "<script",
            "foreignObject",
            "unsafe-",
            "data:image",
            "<image",
        ] {
            assert!(!sanitized.contains(forbidden), "{forbidden}");
        }
        assert!(sanitized.contains("#ff0000"));
    }

    #[test]
    fn animated_images_are_flattened_to_one_bounded_frame() {
        let mut encoded = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                encoder
                    .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                        2,
                        2,
                        image::Rgba(color),
                    )))
                    .unwrap();
            }
        }
        let media = decode_image("image/gif", encoded).unwrap();
        assert_eq!(media.image.format, ImageFormat::Png);
        let decoded = image::load_from_memory(&media.image.bytes)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }
}
