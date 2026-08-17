//! One-time local→synced profile import: correctness, chat2 lineage of
//! imported docs, journal carry-over, idempotence, and the uploads read-root
//! marker.

use std::sync::Arc;

use zeron_doc::{MessagePart, MessageRole, SessionDoc, SessionMessageEntry};

use zeron_engine::local_import::{ImportEvent, marker_grants_read_root};
use zeron_engine::run_journal::journal_paths;
use zeron_engine::{EngineCore, EngineProfile, HarnessId, default_registry};

fn assemble(profile: EngineProfile) -> EngineCore {
    EngineCore::assemble_with_profile(profile, Arc::new(default_registry()), HarnessId::Mock, None)
        .expect("assemble profile")
}

/// Seed a local profile with a space, two chats (one with a doc + journal,
/// one row-only), exactly as a local-first stretch leaves them on disk.
async fn seed_local(data_dir: &std::path::Path) -> (String, String, String) {
    let local = assemble(EngineProfile::local(data_dir).expect("local profile"));
    let device = local.device_id.clone();

    local
        .workspace
        .create_space(
            "space-1",
            &device,
            "/tmp/proj",
            Some("Project".into()),
            false,
        )
        .expect("create space");
    local
        .workspace
        .create_chat("chat-doc", Some("space-1"), None, None, None)
        .expect("create chat with doc");
    local
        .workspace
        .rename_chat("chat-doc", "Fix the flaky test")
        .expect("title");
    local
        .workspace
        .create_chat("chat-bare", Some("space-1"), None, None, None)
        .expect("create bare chat");

    // A real transcript doc, saved the way the local runtime saves it.
    let doc = SessionDoc::init("chat-doc").expect("init doc");
    doc.push_message(&SessionMessageEntry {
        id: "m1".into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t1".into(),
            text: "hello from local mode".into(),
        }],
        created_at: 1_700_000_000_000,
        device_id: device.clone(),
        status: None,
        continuation_of: None,
    })
    .expect("push message");
    let bytes = doc.export_snapshot().expect("snapshot");
    let store = zeron_sync::DocsStore::open(data_dir.join("profiles").join("local"))
        .expect("open local store");
    store
        .save_snapshot_with_cursor("chat-doc", &bytes, 0, 2)
        .expect("save doc");
    drop(store);

    // Journal + resume budget as a run leaves them.
    let journals = data_dir.join("profiles").join("local").join("journals");
    std::fs::create_dir_all(&journals).expect("journals dir");
    let (journal, resume) = journal_paths(&journals, "chat-doc");
    std::fs::write(&journal, "{\"seq\":1,\"event\":{}}\n").expect("journal");
    std::fs::write(&resume, "1").expect("resume");

    // A committed attachment under the local uploads root.
    let uploads = data_dir.join("profiles").join("local").join("uploads");
    std::fs::create_dir_all(&uploads).expect("uploads dir");
    std::fs::write(uploads.join("ab12cd34-shot.png"), b"png").expect("attachment");

    local.shutdown().await;
    (device, "chat-doc".into(), "chat-bare".into())
}

fn run_import(core: &EngineCore) -> Vec<ImportEvent> {
    let importer = core.local_import.clone().expect("synced runtime importer");
    let mut events = Vec::new();
    importer.run(|event| events.push(event)).expect("import");
    events
}

fn summary(events: &[ImportEvent]) -> (usize, usize, usize, usize) {
    match events.last().expect("summary event") {
        ImportEvent::Summary {
            imported_chats,
            imported_spaces,
            skipped_chats,
            skipped_spaces,
            errors,
            ..
        } => {
            assert!(errors.is_empty(), "import errors: {errors:?}");
            (
                *imported_chats,
                *imported_spaces,
                *skipped_chats,
                *skipped_spaces,
            )
        }
        other => panic!("last event must be a summary, got {other:?}"),
    }
}

#[tokio::test]
async fn local_work_imports_into_synced_profile_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_device, chat_doc, chat_bare) = seed_local(dir.path()).await;

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));

    let status = synced
        .local_import
        .as_ref()
        .expect("importer")
        .status()
        .expect("status");
    assert_eq!(status.available_chats, 2);
    assert_eq!(status.available_spaces, 1);
    assert!(!status.imported_before);

    let events = run_import(&synced);
    let (imported_chats, imported_spaces, skipped_chats, skipped_spaces) = summary(&events);
    assert_eq!((imported_chats, imported_spaces), (2, 1));
    assert_eq!((skipped_chats, skipped_spaces), (0, 0));

    // Rows landed with chat2 lineage and their content intact.
    let row = synced
        .workspace
        .chat(&chat_doc)
        .expect("read chat")
        .expect("imported chat row");
    assert_eq!(row.title.as_deref(), Some("Fix the flaky test"));
    assert_eq!(row.room_gen, Some(2), "imported chats must be chat2-born");
    assert_eq!(row.space_id.as_deref(), Some("space-1"));
    assert!(
        synced
            .workspace
            .chat(&chat_bare)
            .expect("read bare")
            .is_some(),
        "row-only chats import too"
    );
    assert!(
        synced
            .workspace
            .space("space-1")
            .expect("read space")
            .is_some()
    );

    // Doc bytes present in the synced store in born-chat2 shape (cursor 0,
    // epoch 2) — the shape DocHost pushes from VV zero on first room join.
    let store = zeron_sync::DocsStore::open(dir.path().join("orgs").join("org1").join("user1"))
        .expect("open synced store");
    let (bytes, cursor, epoch) = store
        .load_snapshot_with_cursor(&chat_doc)
        .expect("load")
        .expect("imported doc row");
    assert_eq!((cursor, epoch), (0, 2));
    let source_store = zeron_sync::DocsStore::open(dir.path().join("profiles").join("local"))
        .expect("open source store");
    assert_eq!(
        source_store
            .load_snapshot(&chat_doc)
            .expect("source load")
            .expect("source doc bytes"),
        bytes,
        "imported doc bytes are a verbatim copy of the local snapshot"
    );
    drop((store, source_store));

    // Journal + resume budget carried over.
    let (journal, resume) = journal_paths(
        &dir.path()
            .join("orgs")
            .join("org1")
            .join("user1")
            .join("journals"),
        &chat_doc,
    );
    assert!(journal.is_file(), "journal copied");
    assert!(resume.is_file(), "resume budget copied");

    // Marker recorded, and it grants the local uploads root read-only.
    let granted = marker_grants_read_root(dir.path(), "org1", "user1")
        .expect("marker grants local uploads root");
    assert_eq!(
        granted,
        dir.path().join("profiles").join("local").join("uploads")
    );
    assert!(
        marker_grants_read_root(dir.path(), "org1", "other-user").is_none(),
        "grants are per (org, user)"
    );

    // Second run: structural idempotence — everything is skipped.
    let status = synced
        .local_import
        .as_ref()
        .expect("importer")
        .status()
        .expect("status");
    assert_eq!(status.available_chats, 0);
    assert!(status.imported_before);
    let events = run_import(&synced);
    let (imported_chats, imported_spaces, skipped_chats, skipped_spaces) = summary(&events);
    assert_eq!((imported_chats, imported_spaces), (0, 0));
    assert_eq!((skipped_chats, skipped_spaces), (2, 1));

    synced.shutdown().await;
}

#[tokio::test]
async fn import_without_local_profile_is_a_clean_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));

    let status = synced
        .local_import
        .as_ref()
        .expect("importer")
        .status()
        .expect("status");
    assert_eq!((status.available_chats, status.available_spaces), (0, 0));

    let events = run_import(&synced);
    let (imported_chats, imported_spaces, ..) = summary(&events);
    assert_eq!((imported_chats, imported_spaces), (0, 0));

    synced.shutdown().await;
}

/// Read the summary WITHOUT asserting a clean run (the failure-injection tests
/// need the errors).
fn raw_summary(events: &[ImportEvent]) -> (usize, usize, Vec<String>) {
    match events.last().expect("summary event") {
        ImportEvent::Summary {
            imported_chats,
            skipped_chats,
            errors,
            ..
        } => (*imported_chats, *skipped_chats, errors.clone()),
        other => panic!("last event must be a summary, got {other:?}"),
    }
}

#[tokio::test]
async fn per_item_failures_surface_in_the_summary_and_leave_the_row_retryable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_device, chat_doc, chat_bare) = seed_local(dir.path()).await;

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));

    // Injected failure: the target journals DIRECTORY is replaced by a file,
    // so the journal copy fails for the one chat that has a journal
    // (chat-doc); the journal-less chat-bare is untouched.
    let target_journals = dir
        .path()
        .join("orgs")
        .join("org1")
        .join("user1")
        .join("journals");
    std::fs::remove_dir_all(&target_journals).expect("clear journals dir");
    std::fs::write(&target_journals, b"obstruction").expect("plant journals obstruction");

    let events = run_import(&synced);
    let (imported, _skipped, errors) = raw_summary(&events);
    assert_eq!(imported, 1, "the healthy chat still imports");
    assert_eq!(
        errors.len(),
        1,
        "the failure is reported, not swallowed: {errors:?}"
    );
    assert!(errors[0].contains(&chat_doc), "{errors:?}");

    // The failed chat's ROW must not exist — structural idempotence makes the
    // retry pick it up again instead of skipping a half-imported chat.
    assert!(
        synced
            .workspace
            .chat(&chat_doc)
            .expect("read chat")
            .is_none(),
        "failed chat must stay pending for retry"
    );
    assert!(
        synced
            .workspace
            .chat(&chat_bare)
            .expect("read chat")
            .is_some()
    );

    // Retry after clearing the obstruction: only the failed chat imports.
    std::fs::remove_file(&target_journals).expect("clear obstruction");
    let events = run_import(&synced);
    let (imported, skipped, errors) = raw_summary(&events);
    assert!(errors.is_empty(), "retry is clean: {errors:?}");
    assert_eq!((imported, skipped), (1, 1));

    synced.shutdown().await;
}

#[tokio::test]
async fn corrupt_marker_is_moved_aside_not_clobbered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (..) = seed_local(dir.path()).await;

    std::fs::write(dir.path().join("local-import.json"), b"{ not json").expect("plant corrupt");

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    let events = run_import(&synced);
    let (_, _, errors) = raw_summary(&events);
    assert!(
        errors.is_empty(),
        "a corrupt OLD marker fails nothing: {errors:?}"
    );

    // Evidence preserved, fresh marker valid, grant re-armed.
    assert!(
        dir.path().join("local-import.json.corrupt").is_file(),
        "corrupt bytes moved aside"
    );
    assert!(
        marker_grants_read_root(dir.path(), "org1", "user1").is_some(),
        "rebuilt marker grants the uploads root"
    );
    synced.shutdown().await;
}

#[tokio::test]
async fn marker_persistence_failure_is_an_import_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (..) = seed_local(dir.path()).await;

    // The marker path itself is a directory: the atomic rename must fail and
    // the failure must reach the summary — a grant that only lives in memory
    // breaks imported attachments on the next restart.
    std::fs::create_dir_all(dir.path().join("local-import.json")).expect("plant marker dir");

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    let events = run_import(&synced);
    let (imported, _, errors) = raw_summary(&events);
    assert_eq!(imported, 2, "data still imports");
    assert!(
        errors.iter().any(|e| e.contains("import marker")),
        "marker failure must be reported: {errors:?}"
    );
    synced.shutdown().await;
}

#[tokio::test]
async fn spaces_only_profile_imports_its_spaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let local = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    let device = local.device_id.clone();
    local
        .workspace
        .create_space(
            "space-only",
            &device,
            "/tmp/proj",
            Some("Project".into()),
            false,
        )
        .expect("create space");
    local.shutdown().await;
    drop(local);

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    let status = synced
        .local_import
        .as_ref()
        .expect("importer")
        .status()
        .expect("status");
    assert_eq!((status.available_chats, status.available_spaces), (0, 1));

    let events = run_import(&synced);
    match events.last().expect("summary") {
        ImportEvent::Summary {
            imported_spaces,
            errors,
            ..
        } => {
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(*imported_spaces, 1);
        }
        other => panic!("expected summary, got {other:?}"),
    }
    assert!(
        synced
            .workspace
            .space("space-only")
            .expect("read space")
            .is_some()
    );
    synced.shutdown().await;
}

#[tokio::test]
async fn marker_keeps_one_entry_per_account() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (..) = seed_local(dir.path()).await;

    let first = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    run_import(&first);
    first.shutdown().await;
    drop(first);

    // A second account on the same device imports too; its marker entry must
    // not erase the first account's grant.
    let second = assemble(EngineProfile::synced(dir.path(), "org2", "user2"));
    run_import(&second);
    second.shutdown().await;

    assert!(marker_grants_read_root(dir.path(), "org1", "user1").is_some());
    assert!(marker_grants_read_root(dir.path(), "org2", "user2").is_some());
}

#[tokio::test]
async fn later_local_work_imports_as_a_delta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (device, ..) = seed_local(dir.path()).await;

    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    let events = run_import(&synced);
    assert_eq!(summary(&events).0, 2);
    synced.shutdown().await;
    drop(synced);

    // A later signed-out stretch creates one more local chat…
    let local = assemble(EngineProfile::local(dir.path()).expect("local profile"));
    local
        .workspace
        .create_chat("chat-later", None, Some(&device), None, Some("/tmp".into()))
        .expect("create later chat");
    local.shutdown().await;
    drop(local);

    // …and signing back in imports exactly that delta.
    let synced = assemble(EngineProfile::synced(dir.path(), "org1", "user1"));
    let status = synced
        .local_import
        .as_ref()
        .expect("importer")
        .status()
        .expect("status");
    assert_eq!(status.available_chats, 1);
    assert!(status.imported_before);
    let events = run_import(&synced);
    let (imported_chats, _, skipped_chats, _) = summary(&events);
    assert_eq!(imported_chats, 1);
    assert_eq!(skipped_chats, 2);
    synced.shutdown().await;
}
