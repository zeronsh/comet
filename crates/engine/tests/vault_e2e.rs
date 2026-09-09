//! End-to-end vault control-plane exercise against a REAL edge worker
//! (`wrangler dev --var AUTH_MODE:dev`): two devices set up / pair / seal /
//! open / revoke / rotate, and a third recovers with the kit. The edge holds
//! only ciphertext and public records throughout; every trust decision is
//! made client-side against local pins.
//!
//! Run with:
//!   (cd edge && npx wrangler dev --port 27640 --var AUTH_MODE:dev --local)
//!   ZERON_VAULT_EDGE_URL=http://127.0.0.1:27640 cargo test -p zeron-engine --test vault_e2e
//!
//! Without the env var the test is skipped (no network in unit CI).

use std::sync::Arc;

use zeron_crypto::content::{self, ContentPurpose};
use zeron_crypto::record::UnverifiedRecord;
use zeron_engine::doc_host::EdgeConfig;
use zeron_engine::vault::client::VaultClient;
use zeron_engine::vault::{MemoryProtection, VaultPhase, VaultService, VaultStore, object_id_for};

fn edge_url() -> Option<String> {
    std::env::var("ZERON_VAULT_EDGE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

fn device(dir: &std::path::Path, edge: &str, org: &str, user: &str) -> VaultService {
    let bearer = format!("{user}@{org}");
    let config = EdgeConfig::with_static_token(edge, bearer);
    let client = VaultClient::new(reqwest::Client::new(), config, org);
    let store = VaultStore::new(
        dir,
        format!("{org}/{user}"),
        Box::new(MemoryProtection::new()),
    );
    VaultService::open(store, Some(client), org, user)
}

fn fresh_profile() -> (String, String) {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    (
        format!("org-{}", &nonce[..8]),
        format!("user-{}", &nonce[8..16]),
    )
}

#[tokio::test]
async fn two_devices_pair_seal_open_revoke_and_recover() {
    let Some(edge) = edge_url() else {
        eprintln!("ZERON_VAULT_EDGE_URL unset; skipping live vault e2e");
        return;
    };
    let (org, user) = fresh_profile();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    // ── A: nothing exists yet, set up ────────────────────────────────────
    let a = device(dir_a.path(), &edge, &org, &user);
    let status = a.refresh().await.unwrap();
    assert_eq!(
        status.phase,
        VaultPhase::NotEnrolled {
            remote_vault: false
        }
    );
    let kit = a.setup().await.unwrap();
    assert_eq!(a.status().phase, VaultPhase::RecoveryConfirmationRequired);
    a.confirm_recovery_kit().await.unwrap();
    assert!(a.is_ready(), "{:?}", a.status().phase);
    assert_eq!(kit.kit.split('-').count(), 11);
    assert!(a.setup().await.is_err(), "second setup is refused");

    // ── B: pairs through the untrusted relay with a comparison code ─────
    let b = device(dir_b.path(), &edge, &org, &user);
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::NotEnrolled { remote_vault: true });
    assert!(
        b.setup().await.is_err(),
        "cannot create a second vault over an existing one"
    );
    let (request_id, code_on_b) = b.request_enrollment().await.unwrap();
    assert!(matches!(b.status().phase, VaultPhase::Pending { .. }));
    let pending = a.pending_requests().await.unwrap();
    let request = pending
        .iter()
        .find(|r| r.request_id == request_id)
        .expect("A sees B's request");
    assert_eq!(
        request.pairing_code, code_on_b,
        "both sides derive the same code"
    );
    // A wrong code (a relay that swapped keys) is refused.
    assert!(a.approve(&request_id, "0000-0000").await.is_err());
    a.approve(&request_id, &code_on_b).await.unwrap();
    // B learns of the approval on refresh and becomes Ready.
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Ready, "{status:?}");
    assert_eq!(status.devices.len(), 2);

    // ── A seals, B opens (object key published through the control plane)
    let object = object_id_for("chat", "chat-e2e");
    let material = a.seal_material(object).await.unwrap();
    let sealed = content::seal(
        &material.binding,
        ContentPurpose::ChatUpdate,
        &material.key,
        &material.signer,
        b"private canary from A",
        1024,
    )
    .unwrap();
    let untrusted = *UnverifiedRecord::parse(sealed.encoded(), 2048)
        .unwrap()
        .untrusted_binding();
    let context = b.open_material(object, &untrusted).await.unwrap();
    let opened = content::open(
        sealed.encoded(),
        &context.binding,
        ContentPurpose::ChatUpdate,
        &context.key,
        &context.author_public_key,
        1024,
    )
    .unwrap();
    assert_eq!(opened.plaintext().as_bytes(), b"private canary from A");
    // Both writers converge on ONE key per object/epoch (first writer wins).
    let material_b = b.seal_material(object).await.unwrap();
    assert_eq!(material_b.key.identifier(), material.key.identifier());

    // ── A revokes B: fresh epoch; B is out, A still seals under epoch 2 ──
    let b_id = b.status().device_id.clone().unwrap();
    a.revoke(&b_id).await.unwrap();
    let status = a.status();
    assert_eq!(status.epoch, Some(2));
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Revoked, "{status:?}");
    let material2 = a.seal_material(object).await.unwrap();
    assert_eq!(material2.binding.epoch, 2);
    assert_ne!(
        material2.key.identifier(),
        material.key.identifier(),
        "new epoch, new object key"
    );
    let sealed2 = content::seal(
        &material2.binding,
        ContentPurpose::ChatUpdate,
        &material2.key,
        &material2.signer,
        b"after revocation",
        1024,
    )
    .unwrap();
    let untrusted2 = *UnverifiedRecord::parse(sealed2.encoded(), 2048)
        .unwrap()
        .untrusted_binding();
    // B (revoked) cannot obtain epoch-2 material; its historical epoch-1
    // material still opens the earlier record (accepted history).
    assert!(b.open_material(object, &untrusted2).await.is_err());
    assert!(b.open_material(object, &untrusted).await.is_ok());

    // ── C recovers with the kit (no existing device involved) ───────────
    let c = device(dir_c.path(), &edge, &org, &user);
    assert!(
        c.recover(
            "AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA",
            None
        )
        .await
        .is_err()
    );
    let genesis = kit.recovery_file["genesisHash"].as_str().map(|h| {
        zeron_engine::vault::store::Hex(h.to_string())
            .decode::<32>()
            .unwrap()
    });
    c.recover(&kit.kit, genesis).await.unwrap();
    assert!(c.is_ready(), "{:?}", c.status().phase);
    assert_eq!(c.status().epoch, Some(3), "recovery is a fresh epoch");
    // C holds history: opens A's epoch-1 and epoch-2 records.
    let context = c.open_material(object, &untrusted2).await.unwrap();
    let opened = content::open(
        sealed2.encoded(),
        &context.binding,
        ContentPurpose::ChatUpdate,
        &context.key,
        &context.author_public_key,
        1024,
    )
    .unwrap();
    assert_eq!(opened.plaintext().as_bytes(), b"after revocation");
    assert!(c.open_material(object, &untrusted).await.is_ok());
    // A catches up to epoch 3 through the recovery envelope C published.
    let status = a.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Ready, "{status:?}");
    assert_eq!(status.epoch, Some(3));
    let material3 = a.seal_material(object).await.unwrap();
    assert_eq!(material3.binding.epoch, 3);

    // ── Persistence: reopening C's store restores trust without the network
    let c_again = device(dir_c.path(), &edge, &org, &user);
    let _ = c_again; // fresh MemoryProtection cannot open the file → Locked, never plaintext
    let locked = VaultService::open(
        VaultStore::new(
            dir_c.path(),
            format!("{org}/{user}"),
            Box::new(MemoryProtection::new()),
        ),
        None,
        &org,
        &user,
    );
    assert!(matches!(
        locked.status().phase,
        VaultPhase::Unavailable { .. } | VaultPhase::Locked { .. }
    ));
    drop(Arc::new(()));
}

/// The authenticated device channel (RFC 0001 §10) through a REAL DeviceRoom
/// DO: two paired members speak RPC over Noise XX; the relay refuses a
/// plaintext frame for the encrypted profile; a stranger (its own vault,
/// same user) cannot complete the handshake; a revoked member is cut off.
#[tokio::test]
async fn device_channel_over_the_live_relay() {
    use zeron_rpc::{
        ChannelHost, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig, RpcError, RpcReply,
        RpcService, StaticToken, methods,
    };

    struct Probe;
    #[async_trait::async_trait]
    impl RpcService for Probe {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::LIST_HARNESSES => Ok(RpcReply::Value(serde_json::json!([]))),
                "Echo" => Ok(RpcReply::Value(params)),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    let Some(edge) = edge_url() else {
        eprintln!("ZERON_VAULT_EDGE_URL unset; skipping live vault e2e");
        return;
    };
    let (org, user) = fresh_profile();
    let bearer = format!("{user}@{org}");
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = device(dir_a.path(), &edge, &org, &user);
    a.refresh().await.unwrap();
    let _kit = a.setup().await.unwrap();
    a.confirm_recovery_kit().await.unwrap();
    let b = device(dir_b.path(), &edge, &org, &user);
    b.refresh().await.unwrap();
    let (request_id, code) = b.request_enrollment().await.unwrap();
    a.approve(&request_id, &code).await.unwrap();
    b.refresh().await.unwrap();
    assert!(b.is_ready());
    // A learns B is a member (its head advanced on approval).
    a.refresh().await.unwrap();

    // A hosts its device room with the vault as channel authority.
    let relay_device = format!("chan-live-{}", uuid::Uuid::new_v4().simple());
    let mut host_config = HostRelayConfig::new(
        edge.clone(),
        relay_device.clone(),
        Arc::new(StaticToken(bearer.clone())),
    );
    host_config.retry = std::time::Duration::from_millis(500);
    host_config.channel = Some(ChannelHost {
        authority: Arc::new(a.clone()),
        service: Arc::new(Probe),
    });
    let _host = HostRelay::spawn(host_config, Arc::new(Probe), Arc::new(|_| {}));

    // B dials through the channel and gets an answer.
    let mut link_config = LinkCacheConfig::new(edge.clone(), Arc::new(StaticToken(bearer.clone())));
    link_config.probe_timeout = std::time::Duration::from_secs(5);
    link_config.cooldown_base = std::time::Duration::from_millis(200);
    link_config.cooldown_max = std::time::Duration::from_millis(200);
    link_config.channel = Some(Arc::new(b.clone()));
    let links = LinkCache::new(link_config);
    let client = loop {
        match links.client(&relay_device).await {
            Ok(client) => break client,
            Err(err) => {
                eprintln!("dial retry: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
    };
    let echoed = client
        .call(
            "Echo",
            serde_json::json!({ "private": "canary over the relay" }),
        )
        .await
        .unwrap();
    assert_eq!(echoed["private"], "canary over the relay");

    // A plaintext client for the same profile: the relay closes the socket
    // (4403) before the host ever sees the frame — the dial fails.
    let mut plain_config =
        LinkCacheConfig::new(edge.clone(), Arc::new(StaticToken(bearer.clone())));
    plain_config.probe_timeout = std::time::Duration::from_secs(5);
    let plain = LinkCache::new(plain_config);
    let err = match plain.client(&relay_device).await {
        Ok(_) => panic!("plaintext relay client must be refused"),
        Err(err) => err.to_string(),
    };
    eprintln!("plaintext dial refused: {err}");

    // A stranger: same user at the relay, but its own vault (another org)
    // — the handshake prologue and membership both refuse it.
    let dir_s = tempfile::tempdir().unwrap();
    let (other_org, _) = fresh_profile();
    let stranger = device(dir_s.path(), &edge, &other_org, &user);
    stranger.refresh().await.unwrap();
    stranger.setup().await.unwrap();
    stranger.confirm_recovery_kit().await.unwrap();
    let mut stranger_config =
        LinkCacheConfig::new(edge.clone(), Arc::new(StaticToken(bearer.clone())));
    stranger_config.probe_timeout = std::time::Duration::from_secs(5);
    stranger_config.channel = Some(Arc::new(stranger.clone()));
    let stranger_links = LinkCache::new(stranger_config);
    assert!(
        stranger_links.client(&relay_device).await.is_err(),
        "a device from another vault must not get a channel"
    );

    // Revocation ends B's session: A's next refresh sees B gone and the
    // established channel is cut at the next frame; a redial is refused.
    let b_id = b.status().device_id.clone().unwrap();
    a.revoke(&b_id).await.unwrap();
    let err = client
        .call("Echo", serde_json::json!({}))
        .await
        .expect_err("revoked member gets no answer");
    eprintln!("post-revocation call: {err}");
    links.invalidate(&relay_device);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        links.client(&relay_device).await.is_err(),
        "revoked member must not re-establish the channel"
    );
}

/// Registry field envelopes (RFC 0001 §9) through the live control plane:
/// one object key per epoch for the whole registry, values bound to their
/// row/field/clock, deletion markers, and key-unavailable withholding.
#[tokio::test]
async fn registry_fields_seal_open_and_bind_their_slot() {
    use zeron_engine::vault::VaultRegistryCodec;
    use zeron_sync::{FieldOpenFailure, RegistryCodec};

    let Some(edge) = edge_url() else {
        eprintln!("ZERON_VAULT_EDGE_URL unset; skipping live vault e2e");
        return;
    };
    let (org, user) = fresh_profile();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = device(dir_a.path(), &edge, &org, &user);
    a.refresh().await.unwrap();
    let _kit = a.setup().await.unwrap();
    a.confirm_recovery_kit().await.unwrap();
    let b = device(dir_b.path(), &edge, &org, &user);
    b.refresh().await.unwrap();
    let (request_id, code) = b.request_enrollment().await.unwrap();
    a.approve(&request_id, &code).await.unwrap();
    b.refresh().await.unwrap();
    assert!(b.is_ready());

    let codec_a = VaultRegistryCodec::new(a.clone(), &user);
    let codec_b = VaultRegistryCodec::new(b.clone(), &user);
    // Sealing before `prepare` is refused (no half-sealed batches).
    assert!(
        codec_a
            .seal_field(
                "chats",
                "c1",
                "title",
                "1-000000-a",
                &serde_json::json!("t")
            )
            .is_err()
    );
    codec_a.prepare().await.unwrap();
    let title = codec_a
        .seal_field(
            "chats",
            "c1",
            "title",
            "0000000000001-000000-a",
            &serde_json::json!("secret title"),
        )
        .unwrap();
    let deleted = codec_a
        .seal_field(
            "chats",
            "c1",
            "branch",
            "0000000000002-000000-a",
            &serde_json::Value::Null,
        )
        .unwrap();
    let wire = serde_json::to_string(&title).unwrap();
    assert!(
        !wire.contains("secret title"),
        "plaintext on the wire: {wire}"
    );
    assert!(title.get("e1").is_some());

    // B opens both after fetching the object key from the control plane.
    let first = codec_b.open_field("chats", "c1", "title", "0000000000001-000000-a", &title);
    if first == Err(FieldOpenFailure::KeyUnavailable) {
        // The synchronous path spawned a key fetch; wait for it.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if codec_b
                .open_field("chats", "c1", "title", "0000000000001-000000-a", &title)
                .is_ok()
            {
                break;
            }
        }
    }
    assert_eq!(
        codec_b.open_field("chats", "c1", "title", "0000000000001-000000-a", &title),
        Ok(Some(serde_json::json!("secret title")))
    );
    assert_eq!(
        codec_b.open_field("chats", "c1", "branch", "0000000000002-000000-a", &deleted),
        Ok(None),
        "an authenticated deletion marker opens as absence"
    );
    // Moved between fields, rows, or clocks: rejected, never displayed.
    assert_eq!(
        codec_b.open_field("chats", "c1", "cwd", "0000000000001-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    assert_eq!(
        codec_b.open_field("chats", "c2", "title", "0000000000001-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    assert_eq!(
        codec_b.open_field("chats", "c1", "title", "0000000000009-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    // Plaintext where ciphertext is required is rejected too.
    assert_eq!(
        codec_b.open_field(
            "chats",
            "c1",
            "title",
            "0000000000001-000000-a",
            &serde_json::json!("plain")
        ),
        Err(FieldOpenFailure::Rejected)
    );
}

/// Opt-in companion for iOS MobileVaultLiveTests. Approval is automatic ONLY
/// inside this fresh, disposable test profile; never run with a real account.
#[tokio::test]
async fn mobile_test_host() {
    use zeron_rpc::{
        ChannelHost, HostRelay, HostRelayConfig, RpcError, RpcReply, RpcService, StaticToken,
    };
    let Ok(directory) = std::env::var("ZERON_MOBILE_E2E_DIR") else {
        return;
    };
    let edge = edge_url().expect("ZERON_VAULT_EDGE_URL is required");
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    let (org, user) = fresh_profile();
    let dir = tempfile::tempdir().unwrap();
    let host = device(dir.path(), &edge, &org, &user);
    host.refresh().await.unwrap();
    host.setup().await.unwrap();
    host.confirm_recovery_kit().await.unwrap();
    struct Echo;
    #[async_trait::async_trait]
    impl RpcService for Echo {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "EchoStream" => Ok(RpcReply::Stream(Box::pin(futures::stream::iter(
                    (0..3).map(|i| serde_json::json!({ "text": format!("stream {i}") })),
                )))),
                _ => Err(RpcError::UnknownMethod(method.into())),
            }
        }
    }
    let relay_id = format!("ios-live-{}", uuid::Uuid::new_v4().simple());
    let mut config = HostRelayConfig::new(
        edge.clone(),
        relay_id.clone(),
        Arc::new(StaticToken(format!("{user}@{org}"))),
    );
    config.channel = Some(ChannelHost {
        authority: Arc::new(host.clone()),
        service: Arc::new(Echo),
    });
    let _relay = HostRelay::spawn(config, Arc::new(Echo), Arc::new(|_| {}));
    let chat = &format!("mobile-sidecar-{}", uuid::Uuid::new_v4().simple());
    let material = host
        .seal_material(object_id_for("chat", chat))
        .await
        .unwrap();
    let tail = serde_json::json!({"chatId":chat,"schemaVersion":1,"totalMessages":1,"updatedAt":1000,
        "messages":[{"id":"message-1","role":"assistant","createdAt":1000,"deviceId":"host","parts":[
            {"id":"text-1","kind":"text","text":"Encrypted recent messages"},
            {"id":"tool-1","kind":"tool","call":{"kind":"exec","command":"echo hello"},"isError":false,
             "resolved":true,"output":"hello","outputRef":format!("{chat}/tool-1")}]}]});
    for (path, purpose, plaintext) in [
        (
            format!("chat2/{chat}-e1/tail"),
            ContentPurpose::Tail,
            serde_json::to_vec(&tail).unwrap(),
        ),
        (
            format!("blob/{chat}/tool-1"),
            ContentPurpose::Blob,
            b"Full encrypted tool output".to_vec(),
        ),
    ] {
        let sealed = content::seal(
            &material.binding,
            purpose,
            &material.key,
            &material.signer,
            &plaintext,
            4 * 1024 * 1024 - 1024,
        )
        .unwrap();
        let response = reqwest::Client::new()
            .put(format!("{edge}/{path}"))
            .bearer_auth(format!("{user}@{org}"))
            .header("content-type", "application/octet-stream")
            .body(sealed.encoded().to_vec())
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "sidecar {path}: {}",
            response.status()
        );
    }
    let connection = serde_json::json!({"edge":edge,"org":org,"user":user,"relay":relay_id,
        "fingerprint":host.status().genesis_hash,"chat":chat});
    std::fs::write(
        directory.join("connection.json"),
        serde_json::to_vec(&connection).unwrap(),
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    let mut approved = None;
    while tokio::time::Instant::now() < deadline {
        if directory.join("done").exists() {
            return;
        }
        for request in host.pending_requests().await.unwrap() {
            host.approve(&request.request_id, &request.pairing_code)
                .await
                .unwrap();
            approved = Some(request.device_id);
        }
        if directory.join("revoke").exists()
            && let Some(id) = approved.take()
        {
            host.revoke(&id).await.unwrap();
            std::fs::write(directory.join("revoked"), b"ok").unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("iOS test did not finish before deadline");
}

#[tokio::test]
async fn running_workspace_switches_to_encrypted_registry_after_setup() {
    use zeron_engine::workspace_host::{WorkspaceHost, WorkspaceHostConfig};
    let Some(edge) = edge_url() else { return };
    let (org, user) = fresh_profile();
    let dir = tempfile::tempdir().unwrap();
    let vault = device(dir.path(), &edge, &org, &user);
    vault.refresh().await.unwrap();
    let workspace = WorkspaceHost::open(
        Arc::new(zeron_sync::DocsStore::open(&dir.path().join("docs")).unwrap()),
        WorkspaceHostConfig {
            device_id: "laptop".into(),
            device_name: "Work laptop".into(),
            platform: "macos".into(),
            org_id: org.clone(),
            user_id: user.clone(),
            vault: Some(vault.clone()),
            edge: Some(EdgeConfig::with_static_token(
                &edge,
                format!("{user}@{org}"),
            )),
        },
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !workspace.connected() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    vault.setup().await.unwrap();
    vault.confirm_recovery_kit().await.unwrap();
    let client = reqwest::Client::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let response = client
                .get(format!("{edge}/registry/{org}/e1/rows"))
                .bearer_auth(format!("{user}@{org}"))
                .send()
                .await
                .unwrap();
            let body: serde_json::Value = response.json().await.unwrap();
            if workspace.connected() && body.to_string().contains("vaultDeviceId") {
                assert!(!body.to_string().contains("Work laptop"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        workspace.read_devices().unwrap()[0].vault_device_id,
        vault.status().device_id
    );
    workspace.shutdown();
}

#[tokio::test]
async fn plaintext_history_migrates_resumably_with_sidecars_and_lineage() {
    use base64::Engine as _;
    use zeron_engine::doc_host::{DocHost, DocHostConfig};
    use zeron_engine::workspace_host::{WorkspaceHost, WorkspaceHostConfig};
    let Some(edge) = edge_url() else { return };
    let (org, user) = fresh_profile();
    let dir = tempfile::tempdir().unwrap();
    let vault = device(dir.path(), &edge, &org, &user);
    let config = EdgeConfig::with_static_token(&edge, format!("{user}@{org}"));
    let store = Arc::new(zeron_sync::DocsStore::open(dir.path().join("docs")).unwrap());
    let workspace = WorkspaceHost::open(
        store.clone(),
        WorkspaceHostConfig {
            device_id: "laptop".into(),
            device_name: "Laptop".into(),
            platform: "macos".into(),
            org_id: org.clone(),
            user_id: user.clone(),
            vault: Some(vault.clone()),
            edge: Some(config.clone()),
        },
    )
    .unwrap();
    let chat = format!("migrate-{}", uuid::Uuid::new_v4().simple());
    workspace
        .create_chat(&chat, None, Some("laptop"), None, None)
        .unwrap();
    let source = zeron_doc::SessionDoc::init(&chat).unwrap();
    let message = |id: &str, text: &str| {
        serde_json::from_value::<zeron_doc::SessionMessageEntry>(serde_json::json!({
            "id":id,"role":"user","parts":[{"id":format!("{id}-text"),"kind":"text","text":text}],
            "createdAt":1234,"deviceId":"laptop"
        }))
        .unwrap()
    };
    source
        .push_message(&message("before", "Existing private history"))
        .unwrap();
    // A real >1 MiB checkpoint must not be shoved into the relay's row path.
    use sha2::{Digest, Sha256};
    let mut large = String::new();
    for n in 0..50_000u64 {
        for byte in Sha256::digest(n.to_le_bytes()) {
            large.push_str(&format!("{byte:02x}"));
        }
    }
    source
        .doc()
        .get_map("migrationFixture")
        .insert("largeHistory", large)
        .unwrap();
    source.doc().commit();
    store
        .save_snapshot_with_cursor(&chat, &source.export_snapshot().unwrap(), 0, 2)
        .unwrap();
    store.mark_processed("already-run").unwrap();
    source
        .push_message(&message("remote", "Only present on the relay"))
        .unwrap();
    source
        .doc()
        .get_map("migrationFixture")
        .insert("outputRef", format!("{chat}/tool-output"))
        .unwrap();
    source.doc().commit();
    let plaintext_checkpoint = source.export_snapshot().unwrap();
    assert!(plaintext_checkpoint.len() > 1024 * 1024);
    let frontier = source.doc().oplog_vv();
    let http = reqwest::Client::new();
    let auth = format!("{user}@{org}");
    let response = http
        .post(format!("{edge}/chat2/{chat}/checkpoint?seqCovered=0"))
        .bearer_auth(&auth)
        .header(
            "x-chat2-frontier",
            base64::engine::general_purpose::STANDARD.encode(frontier.encode()),
        )
        .body(plaintext_checkpoint.clone())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    source
        .push_message(&message("last-row", "Final plaintext row"))
        .unwrap();
    let update = source
        .doc()
        .export(loro::ExportMode::updates(&frontier))
        .unwrap();
    let response = http
        .post(format!(
            "{edge}/chat2/{chat}/rows?batchId=source-final-row&device=laptop"
        ))
        .bearer_auth(&auth)
        .body(update)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let kit = vault.setup().await.unwrap();
    vault.confirm_recovery_kit().await.unwrap();
    let host = DocHost::new(
        store.clone(),
        DocHostConfig {
            device_id: "laptop".into(),
            default_harness: zeron_proto::HarnessId::ClaudeCode,
            edge: Some(config.clone()),
        },
    );
    host.set_vault(vault.clone());
    host.set_workspace(workspace.clone());
    // Missing referenced output must pause migration, not silently drop it.
    host.migrate_history().await.unwrap_err();
    assert_eq!(host.history_migration_status().phase, "paused");
    assert_eq!(host.history_migration_status().completed, 0);
    host.shutdown_workers().await;
    // A reader already waiting in the encrypted room must reconnect when
    // migration seeds a checkpoint without adding any log rows.
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let ws_url = format!("{}/chat2/{chat}-e1/ws", edge.replacen("http", "ws", 1));
    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {auth}").parse().unwrap());
    let (mut reader, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let host = DocHost::new(
        store.clone(),
        DocHostConfig {
            device_id: "laptop".into(),
            default_harness: zeron_proto::HarnessId::ClaudeCode,
            edge: Some(config.clone()),
        },
    );
    host.set_vault(vault.clone());
    host.set_workspace(workspace.clone());
    let codec = zeron_engine::chat2_host::ChatCodec::new(vault.clone(), &chat);
    let restored_blob = codec
        .seal(ContentPurpose::Blob, b"Recovered full tool output", 1024)
        .await
        .unwrap();
    assert!(
        http.put(format!("{edge}/blob/{chat}/tool-output"))
            .bearer_auth(&auth)
            .body(restored_blob.encoded().to_vec())
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    host.migrate_history().await.unwrap();
    assert_eq!(host.history_migration_status().phase, "complete");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(frame) = reader.next().await {
            if let tokio_tungstenite::tungstenite::Message::Close(Some(close)) = frame.unwrap() {
                assert_eq!(u16::from(close.code), 4411);
                return;
            }
        }
        panic!("migration did not refresh the existing reader");
    })
    .await
    .unwrap();
    assert_eq!(host.history_migration_status().completed, 1);
    let sealed = http
        .get(format!("{edge}/chat2/{chat}-e1/checkpoint"))
        .bearer_auth(&auth)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(!sealed.windows(24).any(|w| w == b"Existing private history"));
    let stale_frontier = codec
        .seal(ContentPurpose::Frontier, b"stale", 1024)
        .await
        .unwrap();
    let conflict = http
        .post(format!(
            "{edge}/chat2/{chat}-e1/checkpoint?seqCovered=0&refreshReaders=1"
        ))
        .bearer_auth(&auth)
        .header(
            "x-chat2-frontier",
            base64::engine::general_purpose::STANDARD.encode(stale_frontier.encoded()),
        )
        .header("x-chat2-expected-frontier", "")
        .body(sealed.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    let opened = codec
        .open_async(
            ContentPurpose::Checkpoint,
            &sealed,
            content::MAX_PLAINTEXT_BYTES,
        )
        .await
        .unwrap();
    let restored = loro::LoroDoc::new();
    restored.import(&opened).unwrap();
    assert!(restored.oplog_vv().includes_vv(&source.doc().oplog_vv()));
    let restored = zeron_doc::SessionDoc::from_doc(restored);
    assert_eq!(restored.read_entries().unwrap().len(), 3);
    assert!(store.is_processed("already-run").unwrap());
    assert_eq!(
        host.fetch_tool_blob(&format!("{chat}/tool-output"))
            .await
            .unwrap(),
        "Recovered full tool output"
    );
    let retained = http
        .get(format!("{edge}/chat2/{chat}/checkpoint"))
        .bearer_auth(&auth)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(retained.as_ref(), plaintext_checkpoint.as_slice());
    host.migrate_history().await.unwrap();
    assert_eq!(
        host.open(&chat)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        3
    );
    let recovered_dir = tempfile::tempdir().unwrap();
    let recovered = device(recovered_dir.path(), &edge, &org, &user);
    let genesis =
        zeron_engine::vault::store::Hex(kit.recovery_file["genesisHash"].as_str().unwrap().into())
            .decode::<32>()
            .unwrap();
    recovered.recover(&kit.kit, Some(genesis)).await.unwrap();
    let recovery_codec = zeron_engine::chat2_host::ChatCodec::new(recovered, &chat);
    let recovered_bytes = recovery_codec
        .open_async(
            ContentPurpose::Checkpoint,
            &sealed,
            content::MAX_PLAINTEXT_BYTES,
        )
        .await
        .unwrap();
    assert_eq!(recovered_bytes, opened);
    host.shutdown_workers().await;
}
