//! Windows npm Codex must survive the production registry and ListHarnesses RPC.
#![cfg(windows)]

use zeron_engine::{EngineCore, registry::default_registry};
use zeron_proto::HarnessId;

#[tokio::test]
async fn catalog_child() {
    if std::env::var_os("CODEX_CATALOG_PROBE").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        default_registry().into(),
        HarnessId::Codex,
        None,
    )
    .expect("assemble isolated engine");
    let client = zeron_rpc::memory_client(core.rpc_service());
    let catalog = client
        .call(zeron_rpc::methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .unwrap();
    let codex = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "codex")
        .expect("Codex must be registered");
    assert_eq!(codex["installed"], true, "catalog: {catalog}");
    assert_eq!(
        codex["enabled"], true,
        "composer requires both flags: {catalog}"
    );
    println!("Codex RPC descriptor: {codex}");
}

#[test]
fn npm_codex_is_offered_by_production_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("Node Current");
    let (arch, triple) = if cfg!(target_arch = "aarch64") {
        ("arm64", "aarch64-pc-windows-msvc")
    } else {
        ("x64", "x86_64-pc-windows-msvc")
    };
    let exe = prefix.join(format!("node_modules/@openai/codex/node_modules/@openai/codex-win32-{arch}/vendor/{triple}/bin/codex.exe"));
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(exe, b"MZ").unwrap();
    std::fs::write(prefix.join("codex.cmd"), b"npm shim").unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "catalog_child", "--nocapture"]);
    for key in [
        "HOME",
        "USERPROFILE",
        "FNM_DIR",
        "VOLTA_HOME",
        "PNPM_HOME",
        "APPDATA",
        "CODEX_EXECUTABLE",
    ] {
        command.env_remove(key);
    }
    let output = command
        .env("PATH", &prefix)
        .env("NVM_SYMLINK", &prefix)
        .env("CODEX_CATALOG_PROBE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
