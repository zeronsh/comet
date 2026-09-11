//! Browser-local persistence for the shared settings schemas.
//!
//! `Path` is only an opaque preference namespace on wasm; it is never opened
//! as a browser filesystem path.

use std::path::Path;

#[cfg(target_arch = "wasm32")]
use std::io;

const PREFIX: &str = "zeron.ui.preferences.v1";

fn key(namespace: &Path, name: &str) -> String {
    format!("{PREFIX}:{}:{name}", namespace.to_string_lossy())
}

#[cfg(target_arch = "wasm32")]
fn storage() -> io::Result<web_sys::Storage> {
    web_sys::window()
        .ok_or_else(|| io::Error::other("browser window unavailable"))?
        .local_storage()
        .map_err(|error| io::Error::other(format!("browser local storage: {error:?}")))?
        .ok_or_else(|| io::Error::other("browser local storage unavailable"))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn load(namespace: &Path, name: &str) -> io::Result<Option<String>> {
    storage()?
        .get_item(&key(namespace, name))
        .map_err(|error| io::Error::other(format!("browser local storage read: {error:?}")))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn save(namespace: &Path, name: &str, value: &str) -> io::Result<()> {
    storage()?
        .set_item(&key(namespace, name), value)
        .map_err(|error| io::Error::other(format!("browser local storage write: {error:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_keys_are_stable_and_schema_specific() {
        let namespace = Path::new("comet.ui-wasm-fixture.v1");
        assert_eq!(
            key(namespace, "ui-settings.json"),
            "zeron.ui.preferences.v1:comet.ui-wasm-fixture.v1:ui-settings.json"
        );
        assert_ne!(
            key(namespace, "ui-settings.json"),
            key(namespace, "composer-defaults.json")
        );
    }
}
