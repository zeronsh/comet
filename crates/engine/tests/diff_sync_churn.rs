//! Regression coverage for the diff-sync reconcile runaway (idle checkout,
//! back-to-back `git diff` forever).
//!
//! Reconcile runs on every workspace chat row change — including the writes
//! `sync_entry` itself makes — and used to resolve every chat's checkout
//! identity with fresh `git rev-parse` spawns. One transient spawn failure made
//! the chat ungroupable, so its entry (watchers, checksum state, published
//! diff) was torn down and re-added on the next pass, and every re-add kicked a
//! full capture whose row writes triggered the next reconcile. These tests pin
//! the two dampers: memoized identity resolution and the orphan grace period.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeron_engine::{CheckoutDiffSync, EngineCore, HarnessRegistry};
use zeron_proto::CheckoutDiff;

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Init a repo at `dir` with one committed file and one dirty edit.
async fn init_dirty_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").expect("write a.txt");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
    std::fs::write(dir.join("a.txt"), "one\ntwo\nedited\n").expect("dirty tree");
}

fn assemble(dir: &Path) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    EngineCore::assemble(
        dir,
        Arc::new(HarnessRegistry::new()),
        zeron_proto::HarnessId::Mock,
        None,
    )
    .expect("engine assembles")
}

/// Poll `sync`'s watch until a diff for some checkout appears (or panic).
/// Generous deadline: these tests share the machine with real builds.
async fn wait_for_diff(sync: &CheckoutDiffSync) -> CheckoutDiff {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(diff) = sync.watch_diffs().borrow().first().cloned() {
            return diff;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "diff published before timeout"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn current_diffs(sync: &CheckoutDiffSync) -> Vec<CheckoutDiff> {
    sync.watch_diffs().borrow().clone()
}

/// Block until the workspace chat watch reflects `chat_id`'s presence/absence —
/// row mutations land in the watch asynchronously, and the churn tests need
/// reconcile passes to run against a settled chat list to be deterministic.
async fn wait_chat_state(core: &EngineCore, chat_id: &str, present: bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let found = core
            .workspace
            .watch_chats()
            .borrow()
            .iter()
            .any(|c| c.id == chat_id);
        if found == present {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chat watch settled before timeout"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A transient identity failure (fd exhaustion, EACCES, any failed git spawn)
/// must not tear down a live entry: the memo keeps the chat groupable during
/// chat-watch reconciles, and a failed fresh resolve keeps the memo while the
/// directory still exists. Pre-fix, the first reconcile during the outage
/// removed the entry and the next one re-added it, kicking a fresh capture —
/// the runaway loop.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_survives_transient_identity_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_dirty_repo(&repo_dir).await;

    let core = assemble(&tmp.path().join("data"));
    core.workspace
        .create_space(
            "space-1",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("space row");
    core.workspace
        .create_chat("chat-1", Some("space-1"), None, None, None)
        .expect("chat row");
    wait_chat_state(&core, "chat-1", true).await;
    core.diff_sync.reconcile_now().await;
    let before = wait_for_diff(&core.diff_sync).await;

    // Simulate the outage: every git spawn against the checkout now fails
    // (chdir EACCES), exactly like fd exhaustion did in the incident logs.
    let live = std::fs::metadata(&repo_dir).expect("meta").permissions();
    let mut dead = live.clone();
    dead.set_mode(0o000);
    std::fs::set_permissions(&repo_dir, dead).expect("chmod 000");

    // Chat-watch reconciles during the outage: memo hit, no git needed.
    core.diff_sync.reconcile_now().await;
    core.diff_sync.reconcile_now().await;
    // Repair-tick reconcile during the outage: fresh resolve fails, but the
    // directory still exists, so the memo (and the entry) must survive.
    core.diff_sync.repair_now().await;

    std::fs::set_permissions(&repo_dir, live).expect("chmod back");
    core.diff_sync.reconcile_now().await;

    // Give any (wrongly) kicked capture time to land, then assert nothing
    // changed: same diff, never dropped, never re-published.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = current_diffs(&core.diff_sync);
    assert_eq!(after.len(), 1, "diff must survive the outage");
    assert_eq!(after[0].checkout_id, before.checkout_id);
    assert_eq!(after[0].checksum, before.checksum);
    assert_eq!(
        after[0].updated_at, before.updated_at,
        "entry must not be torn down and re-captured"
    );
    core.shutdown().await;
}

/// A chat-watch emission that briefly misses a chat (row flap) must not tear
/// down the checkout's entry, and the chat coming back must not re-kick a
/// capture. Sustained absence past the grace period must still remove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_flap_keeps_entry_and_sustained_absence_removes_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_dirty_repo(&repo_dir).await;

    let core = assemble(&tmp.path().join("data"));
    // Standalone sync with a tiny grace so removal is testable; the core's own
    // diff_sync (default grace) also runs but assertions target this instance.
    let sync = CheckoutDiffSync::start_with_orphan_grace(
        core.repos.clone(),
        core.workspace.clone(),
        &core.device_id,
        None,
        Duration::from_millis(300),
    );
    core.workspace
        .create_space(
            "space-1",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("space row");
    core.workspace
        .create_chat("chat-1", Some("space-1"), None, None, None)
        .expect("chat row");
    wait_chat_state(&core, "chat-1", true).await;
    sync.reconcile_now().await;
    let before = wait_for_diff(&sync).await;

    // Flap: the chat vanishes for one reconcile pass...
    core.workspace.delete_chat("chat-1").expect("delete chat");
    wait_chat_state(&core, "chat-1", false).await;
    sync.reconcile_now().await;
    let during = current_diffs(&sync);
    assert_eq!(
        during.len(),
        1,
        "one pass without the chat must only mark the entry, not remove it"
    );

    // ...and comes right back. The surviving entry already knows this chat id,
    // so no capture is kicked and nothing is re-published.
    core.workspace
        .create_chat("chat-1", Some("space-1"), None, None, None)
        .expect("chat row again");
    wait_chat_state(&core, "chat-1", true).await;
    sync.reconcile_now().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = current_diffs(&sync);
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].updated_at, before.updated_at,
        "flap-back must not re-capture or re-publish"
    );

    // Genuine removal still works: absent past the grace on consecutive passes.
    core.workspace.delete_chat("chat-1").expect("delete chat");
    wait_chat_state(&core, "chat-1", false).await;
    sync.reconcile_now().await; // marks orphaned
    tokio::time::sleep(Duration::from_millis(400)).await; // > grace
    sync.reconcile_now().await; // removes
    assert!(
        current_diffs(&sync).is_empty(),
        "sustained absence must remove the entry and its diff"
    );
    core.shutdown().await;
}

/// A checkout whose directory is actually gone must still be evicted: the
/// fresh (repair) resolve drops the memo when the cwd no longer exists, and
/// the grace period then runs out. Guards against the dampers making removal
/// impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_checkout_is_evicted_after_grace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_dirty_repo(&repo_dir).await;

    let core = assemble(&tmp.path().join("data"));
    let sync = CheckoutDiffSync::start_with_orphan_grace(
        core.repos.clone(),
        core.workspace.clone(),
        &core.device_id,
        None,
        Duration::from_millis(300),
    );
    core.workspace
        .create_space(
            "space-1",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("space row");
    core.workspace
        .create_chat("chat-1", Some("space-1"), None, None, None)
        .expect("chat row");
    wait_chat_state(&core, "chat-1", true).await;
    sync.reconcile_now().await;
    wait_for_diff(&sync).await;

    // The checkout vanishes while its chat row remains.
    std::fs::remove_dir_all(&repo_dir).expect("remove repo");

    // Chat-watch reconciles keep using the memo (they must not spawn git), so
    // eviction is the repair tick's job: fresh resolve fails + cwd is gone ⇒
    // memo dropped ⇒ chat ungroupable ⇒ orphaned ⇒ removed after grace.
    sync.repair_now().await; // marks orphaned
    tokio::time::sleep(Duration::from_millis(400)).await; // > grace
    sync.repair_now().await; // removes
    assert!(
        current_diffs(&sync).is_empty(),
        "vanished checkout must be evicted once absence outlasts the grace"
    );
    core.shutdown().await;
}
