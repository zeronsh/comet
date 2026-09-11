//! Durable custom-theme library and the bridge into the active runtime registry.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Global};
use zeron_theme::vscode::SourceCompilation;
use zeron_theme::{
    CustomThemeEntry, CustomThemeLibrary, CustomThemeSource, InstallMode, ThemeRegistry,
    replace_custom_families,
};

use crate::appearance;
use crate::theme::Appearance;

pub struct ThemeLibraryState {
    pub data_dir: PathBuf,
    pub library: CustomThemeLibrary,
    pub load_warning: Option<String>,
}

impl Global for ThemeLibraryState {}

pub fn init(data_dir: impl Into<PathBuf>, cx: &mut App) {
    let data_dir = data_dir.into();
    let (library, load_warning) = match load_library(&data_dir) {
        Ok(library) => (library, None),
        Err(error) => {
            tracing::warn!(error = %error, "could not load custom theme library");
            (CustomThemeLibrary::default(), Some(error.to_string()))
        }
    };
    library.install_runtime();
    cx.set_global(ThemeLibraryState {
        data_dir,
        library,
        load_warning,
    });
}

#[cfg(target_arch = "wasm32")]
const BROWSER_THEME_LIBRARY_FILE: &str = "theme-library.json";

fn load_library(data_dir: &Path) -> Result<CustomThemeLibrary> {
    #[cfg(target_arch = "wasm32")]
    {
        let source = crate::settings::browser_storage::load(data_dir, BROWSER_THEME_LIBRARY_FILE)?;
        return Ok(source
            .map(|source| CustomThemeLibrary::from_json(&source))
            .transpose()?
            .unwrap_or_default());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        CustomThemeLibrary::load(data_dir)
    }
}

fn save_library(library: &CustomThemeLibrary, data_dir: &Path) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    {
        crate::settings::browser_storage::save(
            data_dir,
            BROWSER_THEME_LIBRARY_FILE,
            &library.to_json()?,
        )?;
        return Ok(());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        library.save(data_dir)
    }
}

pub fn entries(cx: &App) -> Vec<CustomThemeEntry> {
    cx.try_global::<ThemeLibraryState>()
        .map(|state| state.library.entries.clone())
        .unwrap_or_default()
}

pub fn load_warning(cx: &App) -> Option<String> {
    cx.try_global::<ThemeLibraryState>()
        .and_then(|state| state.load_warning.clone())
}

pub fn compile(path: &Path, family_id: &str, family_name: &str) -> Result<SourceCompilation> {
    CustomThemeLibrary::compile(path, family_id, family_name)
}

pub fn compile_bytes(
    file_name: &str,
    bytes: &[u8],
    family_id: &str,
    family_name: &str,
) -> Result<SourceCompilation> {
    CustomThemeLibrary::compile_bytes(file_name, bytes, family_id, family_name)
}

pub fn install(
    compilation: SourceCompilation,
    selected_variant_ids: &[String],
    mode: InstallMode,
    cx: &mut App,
) -> Result<String> {
    if !cx.has_global::<ThemeLibraryState>() {
        return Err(anyhow!("custom theme library is not initialized"));
    }
    let state = cx.global_mut::<ThemeLibraryState>();
    let mut next = state.library.clone();
    #[cfg(target_arch = "wasm32")]
    let _ = mode;

    #[cfg(target_arch = "wasm32")]
    let mode = InstallMode::Snapshot;
    let id = next.install(compilation, selected_variant_ids, mode)?;
    #[cfg(target_arch = "wasm32")]
    if let Some(entry) = next.entries.iter_mut().find(|entry| entry.id == id) {
        entry.source = CustomThemeSource::ImportedSnapshot {
            imported_from: None,
        };
    }
    persist_and_activate(state, next)?;
    reconcile_and_refresh(cx);
    Ok(id)
}

pub fn reload(id: &str, cx: &mut App) -> Result<()> {
    if !cx.has_global::<ThemeLibraryState>() {
        return Err(anyhow!("custom theme library is not initialized"));
    }
    let state = cx.global_mut::<ThemeLibraryState>();
    let mut next = state.library.clone();
    let result = next.reload(id);
    // Reload failures update the durable warning while deliberately preserving
    // the last known good family.
    persist_and_activate(state, next)?;
    reconcile_and_refresh(cx);
    result
}

pub fn unlink(id: &str, cx: &mut App) -> Result<()> {
    mutate(cx, |library| library.unlink(id))
}

pub fn duplicate_as_snapshot(id: &str, cx: &mut App) -> Result<String> {
    if !cx.has_global::<ThemeLibraryState>() {
        return Err(anyhow!("custom theme library is not initialized"));
    }
    let state = cx.global_mut::<ThemeLibraryState>();
    let mut next = state.library.clone();
    let duplicate = next.duplicate_as_snapshot(id)?;
    persist_and_activate(state, next)?;
    reconcile_and_refresh(cx);
    Ok(duplicate)
}

pub fn duplicate_as_editable(id: &str, cx: &mut App) -> Result<String> {
    if !cx.has_global::<ThemeLibraryState>() {
        return Err(anyhow!("custom theme library is not initialized"));
    }
    let state = cx.global_mut::<ThemeLibraryState>();
    let mut next = state.library.clone();
    let duplicate = next.duplicate_as_editable(id, &state.data_dir)?;
    persist_and_activate(state, next)?;
    reconcile_and_refresh(cx);
    Ok(duplicate)
}

pub fn remove(id: &str, cx: &mut App) -> Result<()> {
    mutate(cx, |library| {
        library
            .remove(id)
            .then_some(())
            .ok_or_else(|| anyhow!("unknown custom theme `{id}`"))
    })
}

pub fn reveal(id: &str, cx: &App) -> Result<()> {
    let path = cx
        .try_global::<ThemeLibraryState>()
        .and_then(|state| state.library.entry(id))
        .and_then(|entry| entry.source.path())
        .ok_or_else(|| anyhow!("theme source has no location to reveal"))?;
    cx.reveal_path(path);
    Ok(())
}

fn mutate<T>(
    cx: &mut App,
    operation: impl FnOnce(&mut CustomThemeLibrary) -> Result<T>,
) -> Result<T> {
    if !cx.has_global::<ThemeLibraryState>() {
        return Err(anyhow!("custom theme library is not initialized"));
    }
    let state = cx.global_mut::<ThemeLibraryState>();
    let mut next = state.library.clone();
    let output = operation(&mut next)?;
    persist_and_activate(state, next)?;
    reconcile_and_refresh(cx);
    Ok(output)
}

fn persist_and_activate(state: &mut ThemeLibraryState, next: CustomThemeLibrary) -> Result<()> {
    save_library(&next, &state.data_dir).context("could not save custom theme library")?;
    replace_custom_families(
        next.entries
            .iter()
            .map(|entry| entry.family.clone())
            .collect(),
    );
    state.library = next;
    state.load_warning = None;
    Ok(())
}

fn reconcile_and_refresh(cx: &mut App) {
    let registry = ThemeRegistry::active();
    let selected = appearance::themes(cx);
    for appearance_kind in [Appearance::Light, Appearance::Dark] {
        let model = if appearance_kind.is_light() {
            zeron_theme::Appearance::Light
        } else {
            zeron_theme::Appearance::Dark
        };
        let selected_id = selected.variant_id(model);
        if registry.variant(selected_id).is_none()
            && let Some(fallback) = registry.variants_for(model).next()
        {
            appearance::set_theme(appearance_kind, fallback.id.clone(), cx);
        }
    }
    appearance::apply_registry_change(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_persistence_leaves_library_state_unchanged() {
        let data_dir = tempfile::tempdir().unwrap();
        let blocking_file = data_dir.path().join("not-a-directory");
        std::fs::write(&blocking_file, "occupied").unwrap();
        let original = CustomThemeLibrary::default();
        let mut state = ThemeLibraryState {
            data_dir: blocking_file,
            library: original.clone(),
            load_warning: Some("previous warning".into()),
        };
        let mut next = original.clone();
        next.entries.push(CustomThemeEntry {
            id: "unpersisted".into(),
            name: "Unpersisted".into(),
            source: zeron_theme::CustomThemeSource::ImportedSnapshot {
                imported_from: None,
            },
            family: zeron_theme::ThemeFamily {
                id: "unpersisted".into(),
                name: "Unpersisted".into(),
                variants: Vec::new(),
            },
            reports: Default::default(),
            selected_variant_ids: Vec::new(),
            status: zeron_theme::CustomThemeStatus::Ready,
        });

        assert!(persist_and_activate(&mut state, next).is_err());
        assert_eq!(state.library, original);
        assert_eq!(state.load_warning.as_deref(), Some("previous warning"));
    }
}
