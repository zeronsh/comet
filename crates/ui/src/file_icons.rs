//! Deterministic file and folder identity icons backed by Symbols.
//!
//! Resolution preserves the VS Code theme's semantics: exact basenames win,
//! followed by the longest matching compound extension, an optional
//! syntax-language or MIME hint, and finally the generic file/folder icon. The
//! authored SVG colors are rendered without tinting.

use std::{borrow::Cow, collections::HashMap, sync::LazyLock};

use gpui::{AssetSource, Img, Result, SharedString, Styled as _, img};
use rust_embed::RustEmbed;
use serde::Deserialize;
use zeron_syntax::LanguageId;

use crate::theme::{Appearance, Theme};

const ASSET_PREFIX: &str = "file-icons/";

/// Neutral seat for a polychrome file icon. Callers choose where a well is
/// appropriate; its counter-shade stays visible in both appearances without
/// tinting the authored artwork. Frost needs more coverage because the
/// backdrop beneath the translucent surface can vary.
pub(crate) fn well_bg(theme: &Theme) -> gpui::Hsla {
    let alpha = if theme.is_frost() { 0.32 } else { 0.16 };
    match theme.appearance {
        Appearance::Dark => crate::theme::grey(0).opacity(alpha),
        Appearance::Light => crate::theme::grey(255).opacity(alpha),
    }
}

#[derive(RustEmbed)]
#[folder = "assets/file-icons"]
struct EmbeddedFileIcons;

/// File-icon half of the application's composite gpui asset source.
pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let Some(path) = path.strip_prefix(ASSET_PREFIX) else {
            return Ok(None);
        };
        Ok(EmbeddedFileIcons::get(path).map(|asset| asset.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(EmbeddedFileIcons::iter()
            .map(|path| format!("{ASSET_PREFIX}{path}"))
            .filter(|asset_path| asset_path.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIconKind {
    File,
    Directory,
    /// The workspace protocol does not currently expose a symlink target kind,
    /// so links resolve by filename and otherwise use the generic file glyph.
    Symlink,
}

#[derive(Debug, Clone, Copy)]
pub struct FileIconIdentity<'a> {
    pub kind: FileIconKind,
    pub name: &'a str,
    pub expanded: bool,
    pub language: Option<LanguageId>,
    pub mime_type: Option<&'a str>,
}

impl<'a> FileIconIdentity<'a> {
    pub fn file(path: &'a str) -> Self {
        Self {
            kind: FileIconKind::File,
            name: path,
            expanded: false,
            language: None,
            mime_type: None,
        }
    }

    pub fn directory(path: &'a str, expanded: bool) -> Self {
        Self {
            kind: FileIconKind::Directory,
            name: path,
            expanded,
            language: None,
            mime_type: None,
        }
    }

    pub fn symlink(path: &'a str) -> Self {
        Self {
            kind: FileIconKind::Symlink,
            name: path,
            expanded: false,
            language: None,
            mime_type: None,
        }
    }

    pub fn with_language(mut self, language: LanguageId) -> Self {
        self.language = Some(language);
        self
    }

    pub fn with_mime_type(mut self, mime_type: &'a str) -> Self {
        self.mime_type = Some(mime_type);
        self
    }
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "iconDefinitions")]
    icon_definitions: HashMap<String, IconDefinition>,
    file: Option<String>,
    folder: Option<String>,
    #[serde(default, rename = "fileExtensions")]
    file_extensions: HashMap<String, String>,
    #[serde(default, rename = "fileNames")]
    file_names: HashMap<String, String>,
    #[serde(default, rename = "folderNames")]
    folder_names: HashMap<String, String>,
    #[serde(default, rename = "languageIds")]
    language_ids: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct IconDefinition {
    #[serde(rename = "iconPath")]
    icon_path: String,
}

static MANIFEST: LazyLock<Manifest> = LazyLock::new(|| {
    let mut manifest: Manifest = serde_json::from_str(include_str!("file-icons.json"))
        .expect("bundled file-icon manifest must be valid");
    manifest.file_extensions = lowercase_keys(manifest.file_extensions);
    manifest.file_names = lowercase_keys(manifest.file_names);
    manifest.folder_names = lowercase_keys(manifest.folder_names);
    manifest
});

fn lowercase_keys(values: HashMap<String, String>) -> HashMap<String, String> {
    values
        .into_iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect()
}

/// Resolve an identity to the asset path registered with gpui.
pub fn asset_path(identity: FileIconIdentity<'_>, _appearance: Appearance) -> SharedString {
    let name = basename(identity.name).to_ascii_lowercase();
    let asset = match identity.kind {
        FileIconKind::Directory => resolve_directory(&name, identity.expanded),
        FileIconKind::File | FileIconKind::Symlink => resolve_file(&name, identity),
    };
    SharedString::from(format!("{ASSET_PREFIX}{asset}"))
}

/// Build a decorative, polychrome file-theme image.
///
/// GPUI's [`gpui::Svg`] element is intentionally monochrome: it extracts only
/// an alpha mask and requires a `text_color` before it paints. Loading the same
/// embedded SVG through [`gpui::img`] rasterizes its authored fills into a
/// polychrome image, preserving the VS Code theme artwork.
pub fn icon(identity: FileIconIdentity<'_>, appearance: Appearance) -> Img {
    img(asset_path(identity, appearance)).flex_none()
}

/// Whether a filename resolves to a themed identity rather than the generic
/// document fallback. This is intentionally stricter than [`asset_path`]: it
/// lets prose renderers decorate a standalone filename without treating every
/// dotted token as a file reference.
pub(crate) fn has_specific_file_icon(path: &str) -> bool {
    let name = basename(path).to_ascii_lowercase();
    let identity = FileIconIdentity::file(path);
    resolve_file(&name, identity) != generic_file_asset()
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn definition_asset(definition: &str) -> Option<&'static str> {
    // These two identifiers are referenced by the upstream manifest but do not
    // have icon definitions. Its language associations reveal the intended
    // aliases, so resolve them rather than falling back to a generic document.
    let definition = match definition {
        "less" => "brackets-sky",
        "yml" => "yaml",
        definition => definition,
    };
    MANIFEST
        .icon_definitions
        .get(definition)?
        .icon_path
        .strip_prefix("./icons/")
}

fn resolve_directory(name: &str, _expanded: bool) -> &'static str {
    MANIFEST
        .folder_names
        .get(name)
        .or(MANIFEST.folder.as_ref())
        .and_then(|definition| definition_asset(definition))
        .unwrap_or("folders/folder.svg")
}

fn resolve_file(name: &str, identity: FileIconIdentity<'_>) -> &'static str {
    if let Some(asset) = MANIFEST
        .file_names
        .get(name)
        .and_then(|definition| definition_asset(definition))
    {
        return asset;
    }

    for extension in compound_extensions(name) {
        if let Some(asset) = MANIFEST
            .file_extensions
            .get(extension)
            .and_then(|definition| definition_asset(definition))
        {
            return asset;
        }
    }

    language_asset(identity.language)
        .or_else(|| mime_asset(identity.mime_type))
        .unwrap_or_else(generic_file_asset)
}

fn generic_file_asset() -> &'static str {
    MANIFEST
        .file
        .as_deref()
        .and_then(definition_asset)
        .unwrap_or("files/document.svg")
}

fn compound_extensions(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('.')
        .map(|(index, _)| &name[index + 1..])
        .filter(|extension| !extension.is_empty())
}

fn language_asset(language: Option<LanguageId>) -> Option<&'static str> {
    let language_id = match language? {
        LanguageId::Rust => "rust",
        LanguageId::JavaScript => "javascript",
        LanguageId::Jsx => "javascriptreact",
        LanguageId::TypeScript => "typescript",
        LanguageId::Tsx => "typescriptreact",
        LanguageId::Python => "python",
        LanguageId::Go => "go",
        LanguageId::Json | LanguageId::Jsonc => "json",
        LanguageId::Bash => "shellscript",
        LanguageId::Toml => return definition_asset("gear"),
        LanguageId::Markdown => "markdown",
        LanguageId::Html => "html",
        LanguageId::Css => "css",
        LanguageId::Yaml => "yaml",
        LanguageId::C => "c",
        LanguageId::Cpp => "cpp",
        LanguageId::CSharp => "csharp",
        LanguageId::Java => "java",
        LanguageId::Kotlin => return definition_asset("kotlin"),
        LanguageId::Swift => "swift",
        LanguageId::Ruby => "ruby",
        LanguageId::Php => "php",
        LanguageId::Sql => "sql",
        LanguageId::Lua => "lua",
        LanguageId::Dockerfile => "dockerfile",
        LanguageId::Nix => "nix",
        LanguageId::Make => "makefile",
    };
    MANIFEST
        .language_ids
        .get(language_id)
        .and_then(|definition| definition_asset(definition))
}

fn mime_asset(mime_type: Option<&str>) -> Option<&'static str> {
    let mime_type = mime_type?.split(';').next()?.trim().to_ascii_lowercase();
    definition_asset(match mime_type.as_str() {
        mime if mime.starts_with("image/") => "image",
        mime if mime.starts_with("audio/") => "audio",
        mime if mime.starts_with("video/") => "video",
        "application/json" | "application/ld+json" => "brackets-yellow",
        "application/pdf" => "pdf",
        "application/zip" | "application/gzip" | "application/x-tar" => "compressed",
        "text/markdown" => "markdown",
        "text/css" => "brackets-sky",
        "text/html" => "brackets-orange",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(identity: FileIconIdentity<'_>) -> SharedString {
        asset_path(identity, Appearance::Dark)
    }

    #[test]
    fn asset_source_preserves_authored_icon_bytes() {
        let path = "files/ts.svg";
        let embedded = EmbeddedFileIcons::get(path).unwrap();
        let loaded = Assets
            .load(&format!("{ASSET_PREFIX}{path}"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.as_ref(), embedded.data.as_ref());
    }

    #[test]
    fn exact_names_are_case_insensitive_and_ignore_parent_dots() {
        assert_eq!(
            path(FileIconIdentity::file("C:\\project\\PACKAGE.JSON")),
            "file-icons/files/node.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("folder.css/unknown")),
            "file-icons/files/document.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("config/.env.local")),
            "file-icons/files/gear.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("docs/claude.md")),
            "file-icons/files/claude.svg"
        );
    }

    #[test]
    fn compound_extensions_win_longest_first() {
        assert_eq!(
            path(FileIconIdentity::file("component.test.tsx")),
            "file-icons/files/react-test.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("model.schema.json")),
            "file-icons/files/brackets-yellow.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("model.freezed.dart")),
            "file-icons/files/dart.svg"
        );
    }

    #[test]
    fn folders_resolve_name_and_expansion_state() {
        assert_eq!(
            path(FileIconIdentity::directory("src", false)),
            "file-icons/folders/folder-orange-code.svg"
        );
        assert_eq!(
            path(FileIconIdentity::directory("src", true)),
            "file-icons/folders/folder-orange-code.svg"
        );
        assert_eq!(
            path(FileIconIdentity::directory("unknown", true)),
            "file-icons/folders/folder.svg"
        );
    }

    #[test]
    fn appearance_independence_and_hints_are_supported() {
        assert_eq!(
            asset_path(FileIconIdentity::file("bun.lock"), Appearance::Light),
            "file-icons/files/bun.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("untitled").with_language(LanguageId::Rust)),
            "file-icons/files/rust.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("download").with_mime_type("image/png; charset=binary")),
            "file-icons/files/image.svg"
        );
    }

    #[test]
    fn symlinks_and_unknown_files_fall_back_cleanly() {
        assert_eq!(
            path(FileIconIdentity::symlink("README.md")),
            "file-icons/files/markdown.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("unknown.unrecognized")),
            "file-icons/files/document.svg"
        );
        assert_eq!(
            path(FileIconIdentity::file("")),
            "file-icons/files/document.svg"
        );
        assert!(has_specific_file_icon("src/component.test.tsx"));
        assert!(has_specific_file_icon("config/.env.local"));
        assert!(!has_specific_file_icon("version.unknown"));
    }

    #[test]
    fn every_manifest_reference_is_embedded_and_every_asset_is_svg() {
        let referenced_definitions = MANIFEST
            .file
            .iter()
            .chain(MANIFEST.folder.iter())
            .chain(MANIFEST.file_extensions.values())
            .chain(MANIFEST.file_names.values())
            .chain(MANIFEST.folder_names.values())
            .chain(MANIFEST.language_ids.values());
        for definition in referenced_definitions {
            assert!(
                definition_asset(definition).is_some(),
                "manifest references missing definition {definition}"
            );
        }

        for (definition, icon) in &MANIFEST.icon_definitions {
            let asset = icon
                .icon_path
                .strip_prefix("./icons/")
                .unwrap_or_else(|| panic!("unexpected icon path for {definition}"));
            let bytes = EmbeddedFileIcons::get(asset)
                .unwrap_or_else(|| panic!("definition {definition} references missing {asset}"));
            let text = std::str::from_utf8(&bytes.data).expect("file icon svg is utf-8");
            assert!(text.contains("<svg"), "{asset} is not an svg");
            assert!(
                text.contains("viewBox") || (text.contains("width=") && text.contains("height=")),
                "{asset} lacks intrinsic dimensions"
            );
        }
    }

    #[test]
    fn complete_theme_bundle_is_registered() {
        let assets = Assets.list(ASSET_PREFIX).unwrap();
        assert!(
            assets.len() >= 350,
            "unexpectedly small icon bundle: {}",
            assets.len()
        );
        for asset in EmbeddedFileIcons::iter().filter(|asset| asset.ends_with(".svg")) {
            let bytes = EmbeddedFileIcons::get(&asset)
                .unwrap_or_else(|| panic!("listed asset is missing: {asset}"));
            let text = std::str::from_utf8(&bytes.data).expect("file icon svg is utf-8");
            assert!(text.contains("<svg"), "{asset} is not an svg");
            assert!(
                text.contains("viewBox") || (text.contains("width=") && text.contains("height=")),
                "{asset} lacks intrinsic dimensions"
            );
        }
        assert!(Assets.list("icons/").unwrap().is_empty());
        assert!(Assets.load("file-icons/nope.svg").unwrap().is_none());
    }
}
