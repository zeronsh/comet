//! `zeron update` — check for and apply a newer release, natively (the same
//! flow `edge/src/install.sh` performs: download → verify → symlink swap →
//! service restart). macOS app bundles swap the bundle instead; source builds
//! are report-only.

use anyhow::bail;
use zeron_update::{InstallKind, current_version, version_newer};

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(edge_url: &str, check_only: bool) -> anyhow::Result<()> {
    let manifest = zeron_update::fetch_latest(edge_url).await?;
    let current = current_version();
    if !version_newer(&manifest.version, current) {
        println!(
            "zeron {current} is up to date (latest: {}).",
            manifest.version
        );
        return Ok(());
    }
    println!("zeron {current} → {} available", manifest.version);
    if check_only {
        std::process::exit(1);
    }

    match zeron_update::detect_install() {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                zeron_update::headless_artifact(&manifest.version)
            );
            zeron_update::stage_headless(edge_url, &manifest, &app_root).await?;
            zeron_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            match zeron_update::restart_service() {
                Ok(()) => println!("engine service restarted."),
                Err(err) => println!(
                    "note: service restart failed ({err:#}) — restart the engine manually to finish."
                ),
            }
            Ok(())
        }
        InstallKind::MacApp { bundle } => {
            println!(
                "downloading {}…",
                zeron_update::mac_app_artifact(&manifest.version)
            );
            let data_dir = super::paths::data_dir();
            let staged = zeron_update::stage_mac_app(edge_url, &manifest, &data_dir).await?;
            zeron_update::apply_mac_app(&staged, &bundle)?;
            println!("updated {} — relaunch Zeron to finish.", bundle.display());
            Ok(())
        }
        InstallKind::Unmanaged => {
            bail!(
                "this binary is not update-managed (source build or hand-copied).\n\
                 Linux: curl -fsSL https://zeron.sh/install.sh | sh\n\
                 macOS: download the new Zeron.app dmg, or rebuild from source.\n\
                 Windows: automatic installation is not supported; rebuild from source."
            )
        }
    }
}
