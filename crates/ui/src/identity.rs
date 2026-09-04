//! Identity tiles: the rounded monogram that stands in for a project or a
//! person wherever there is no uploaded image.
//!
//! The color is derived from the row's own id, not assigned or stored, so the
//! same project is the same color on every device and after every reinstall
//! without a byte of sync. Lightness and saturation come from the resolved
//! theme, so a tile reads at the same weight in light and dark and never
//! fights an imported palette.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Div, Hsla, ObjectFit, SharedString, Window, div, hsla, img, prelude::*, px,
};

use crate::settings::{self, SavePolicy};
use crate::theme::Theme;

/// Largest picture accepted from the file picker. A mark is drawn at 15–64px;
/// anything past this is a mis-pick (a RAW frame, a PSD) and decoding it would
/// stall the frame it lands on.
const MAX_PICTURE_BYTES: u64 = 8 * 1024 * 1024;

/// Formats gpui's image loader handles. Checked by extension AND by the
/// decoder itself — the extension only decides whether it is worth reading.
const PICTURE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// Settings key for a project's picture.
pub fn space_key(space_id: &str) -> String {
    format!("space:{space_id}")
}

/// Settings key for an account's picture.
pub fn user_key(user_id: &str) -> String {
    format!("user:{user_id}")
}

fn images_dir(cx: &App) -> Option<PathBuf> {
    settings::data_dir(cx).map(|dir| dir.join("images"))
}

/// The stored picture for `key`, if one was chosen and the file is still
/// there. A picture deleted underneath us falls back to the monogram rather
/// than painting a broken tile.
pub fn picture(key: &str, cx: &App) -> Option<PathBuf> {
    let name = settings::current(cx).images.get(key)?.clone();
    let path = images_dir(cx)?.join(name);
    path.is_file().then_some(path)
}

/// Copy `src` into the profile's image store and point `key` at it.
///
/// Copied rather than referenced: a picture linked from the Desktop would go
/// missing the first time the user tidied up, and the tile would silently
/// revert. The destination name carries the source's content hash, so
/// re-picking the same file rewrites one entry instead of growing the store.
pub fn set_picture(key: &str, src: &Path, cx: &mut App) -> Result<(), PictureError> {
    let extension = src
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| PICTURE_EXTENSIONS.contains(&e.as_str()))
        .ok_or(PictureError::UnsupportedFormat)?;

    let bytes = std::fs::metadata(src)
        .map_err(|_| PictureError::Unreadable)?
        .len();
    if bytes > MAX_PICTURE_BYTES {
        return Err(PictureError::TooLarge);
    }

    let dir = images_dir(cx).ok_or(PictureError::Unreadable)?;
    std::fs::create_dir_all(&dir).map_err(|_| PictureError::Unreadable)?;

    let data = std::fs::read(src).map_err(|_| PictureError::Unreadable)?;
    let name = format!("{}.{extension}", content_hash(&data));
    let dest = dir.join(&name);
    // Same content, same name: an existing file is already correct.
    if !dest.is_file() {
        std::fs::write(&dest, &data).map_err(|_| PictureError::Unreadable)?;
    }

    let previous = settings::current(cx).images.get(key).cloned();
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.images.insert(key.to_string(), name.clone());
    });
    if let Some(previous) = previous {
        collect_unreferenced(&previous, &dir, cx);
    }
    Ok(())
}

/// Drop `key`'s picture, falling the tile back to its monogram.
pub fn clear_picture(key: &str, cx: &mut App) {
    let Some(previous) = settings::current(cx).images.get(key).cloned() else {
        return;
    };
    settings::update(SavePolicy::Immediate, cx, |settings| {
        settings.images.remove(key);
    });
    if let Some(dir) = images_dir(cx) {
        collect_unreferenced(&previous, &dir, cx);
    }
}

/// Delete a stored file once nothing points at it. Content-addressed names
/// mean two keys can share one file, so the reference check is required —
/// without it, clearing one project's picture would blank another's.
fn collect_unreferenced(name: &str, dir: &Path, cx: &App) {
    if settings::current(cx).images.values().any(|v| v == name) {
        return;
    }
    let _ = std::fs::remove_file(dir.join(name));
}

fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureError {
    UnsupportedFormat,
    TooLarge,
    Unreadable,
}

impl PictureError {
    /// Message shown next to the control that failed. Says the limit, not just
    /// that there was one.
    pub fn message(self) -> &'static str {
        match self {
            Self::UnsupportedFormat => "Choose a PNG, JPEG, WebP, or GIF.",
            Self::TooLarge => "That image is over 8 MB.",
            Self::Unreadable => "That file could not be read.",
        }
    }
}

/// Open the system picker and store the result under `key`. `on_done` reports
/// the outcome so the calling page can surface a failure inline; a cancelled
/// picker reports nothing.
pub fn pick_picture<T: 'static>(
    key: String,
    window: &mut Window,
    cx: &mut gpui::Context<T>,
    on_done: impl Fn(&mut T, Result<(), PictureError>, &mut gpui::Context<T>) + 'static,
) {
    let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Choose Image".into()),
    });
    let _ = window;
    cx.spawn(async move |this, cx| {
        let path = match receiver.await {
            Ok(Ok(Some(mut paths))) => paths.pop(),
            _ => None,
        };
        let Some(path) = path else {
            return;
        };
        let _ = this.update(cx, |view, cx| {
            let result = set_picture(&key, &path, cx);
            on_done(view, result, cx);
            cx.notify();
        });
    })
    .detach();
}

/// Distinct hues on the wheel. A prime count avoids landing repeatedly on the
/// same few slots for sequentially-numbered ids, and 17 is far enough apart
/// that neighbors in a sidebar stay tellable at 15px.
const HUES: u64 = 17;

/// Stable hue in `0.0..1.0` for `seed`. FNV-1a: no allocation, no dependency,
/// and identical across platforms and releases — which is the whole point,
/// since the color is never written down anywhere.
pub fn hue(seed: &str) -> f32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    (h % HUES) as f32 / HUES as f32
}

/// The letter a tile shows: the first character of the label, uppercased.
/// Non-alphanumeric leads (a dotfile directory, an emoji-prefixed name) fall
/// through to the first character that carries meaning, then to `·` so the
/// tile is never blank.
pub fn monogram(label: &str) -> SharedString {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string().into())
        .unwrap_or_else(|| SharedString::from("·"))
}

/// A `size`-square rounded-rect tile for a *place* — a project, a folder.
///
/// Callers style nothing: the mark is self-contained so every surface that
/// shows one stays identical without copying six lines of styling around.
pub fn tile(seed: &str, label: &str, size: f32, theme: &Theme) -> Div {
    mark(seed, label, size, (size * 0.3).max(4.0), theme)
}

/// The circular variant, for a *person* — an account, a device owner. Same
/// derivation, so one identity reads the same wherever it appears.
pub fn avatar(seed: &str, label: &str, size: f32, theme: &Theme) -> Div {
    mark(seed, label, size, size / 2.0, theme)
}

/// [`tile`], showing `key`'s chosen picture when there is one.
pub fn tile_for(
    key: &str,
    seed: &str,
    label: &str,
    size: f32,
    theme: &Theme,
    cx: &App,
) -> AnyElement {
    match picture(key, cx) {
        Some(path) => framed(path, size, (size * 0.3).max(4.0)),
        None => tile(seed, label, size, theme).into_any_element(),
    }
}

/// [`avatar`], showing `key`'s chosen picture when there is one.
pub fn avatar_for(
    key: &str,
    seed: &str,
    label: &str,
    size: f32,
    theme: &Theme,
    cx: &App,
) -> AnyElement {
    match picture(key, cx) {
        Some(path) => framed(path, size, size / 2.0),
        None => avatar(seed, label, size, theme).into_any_element(),
    }
}

/// A stored picture cropped into the same square a monogram would occupy.
/// `Cover` rather than `Contain`: a mark is a shape, and letterboxing one
/// leaves bars inside the corner radius.
fn framed(path: PathBuf, size: f32, radius: f32) -> AnyElement {
    img(path)
        .size(px(size))
        .flex_none()
        .rounded(px(radius))
        .object_fit(ObjectFit::Cover)
        .into_any_element()
}

fn mark(seed: &str, label: &str, size: f32, radius: f32, theme: &Theme) -> Div {
    let hue = hue(seed);
    let (fill, text) = fill_and_text(hue, theme);
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(radius))
        .bg(fill)
        .flex()
        .items_center()
        .justify_center()
        // Tracks the tile rather than the type scale: a monogram is a mark,
        // and at 15px a body-sized glyph overflows its own corner radius.
        .text_size(px((size * 0.46).max(9.0)))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(text)
        .child(monogram(label))
}

/// Tile fill and its text color. Dark themes get a dim, desaturated wash with
/// a bright glyph; light themes invert that, so contrast holds either way
/// without measuring the specific palette.
fn fill_and_text(hue: f32, theme: &Theme) -> (Hsla, Hsla) {
    if theme.appearance.is_light() {
        (hsla(hue, 0.52, 0.86, 1.0), hsla(hue, 0.72, 0.28, 1.0))
    } else {
        (hsla(hue, 0.42, 0.26, 1.0), hsla(hue, 0.78, 0.78, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_is_stable_and_bounded() {
        // The contract the whole module rests on: never stored, so it must be
        // reproducible byte-for-byte, on any platform, forever.
        assert_eq!(hue("space-7"), hue("space-7"));
        assert_ne!(hue("space-7"), hue("space-8"));
        for seed in ["", "a", "space-7", "/Users/x/code/very-long-project-name"] {
            let h = hue(seed);
            assert!((0.0..1.0).contains(&h), "{seed} produced {h}");
        }
    }

    #[test]
    fn picture_keys_are_namespaced() {
        // A project and an account can carry the same id string. Without the
        // namespace one would silently adopt the other's picture.
        assert_ne!(space_key("abc"), user_key("abc"));
        assert_eq!(space_key("abc"), "space:abc");
        assert_eq!(user_key("abc"), "user:abc");
    }

    #[test]
    fn content_hash_is_stable_and_content_addressed() {
        // Re-picking the same file must reuse one stored name rather than
        // growing the store, and two different images must never collide onto
        // one name.
        assert_eq!(content_hash(b"same bytes"), content_hash(b"same bytes"));
        assert_ne!(content_hash(b"one image"), content_hash(b"other image"));
        assert_eq!(content_hash(b"").len(), 16);
    }

    #[test]
    fn monogram_skips_punctuation_and_never_blanks() {
        assert_eq!(monogram("zeron"), SharedString::from("Z"));
        assert_eq!(monogram(".dotfiles"), SharedString::from("D"));
        assert_eq!(monogram("  spaced"), SharedString::from("S"));
        assert_eq!(monogram("42-tests"), SharedString::from("4"));
        assert_eq!(monogram(""), SharedString::from("·"));
        assert_eq!(monogram("—"), SharedString::from("·"));
    }
}
