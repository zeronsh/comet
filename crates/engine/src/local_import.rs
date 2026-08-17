//! One-time local→synced profile import.
//!
//! A device that worked locally (PR #30's `profiles/local`) and then signs in
//! gets its chats, spaces, journals, and command-ledger claims carried into
//! the synced profile — through the same write paths every live mutation
//! uses, so nothing here invents a second persistence or sync mechanism:
//!
//!  - chat docs land via `save_snapshot_with_cursor(chat_id, bytes, 0, CHAT2_DOC_EPOCH)`,
//!    which is exactly the "born chat2" shape: cursor 0 makes the first room
//!    join push the doc's full update log from VV zero (doc_host first-contact
//!    push), and epoch 2 keeps `DocHost::open` off the s2 discard-and-adopt
//!    branch that would drop the transcript.
//!  - registry rows go through `WorkspaceHost::import_chat_row` /
//!    `import_space_row` (live upserts: persisted, pushed, watched).
//!  - attachments are NOT copied or rewritten. Transcripts embed absolute
//!    paths under the local profile's uploads root, so that root becomes a
//!    read-only jail root of the synced profile — the exact mechanism the
//!    legacy-uploads adoption already uses (`EngineProfile::claim_legacy_uploads_root`).
//!    The marker file re-arms the root on every later synced boot.
//!
//! Idempotence is structural: a chat/space whose row already exists in the
//! target is skipped, so re-running after a partial import (or after new
//! local work from a later signed-out stretch) imports only what's missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeron_doc::{REGISTRY_DOC_ID, RegistryDoc};
use zeron_sync::DocsStore;

use crate::EngineError;
use crate::chat2_host::CHAT2_DOC_EPOCH;
use crate::run_journal::journal_paths;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;

/// Marker recording completed imports, at `{data_dir}/local-import.json`.
/// One entry per target (org, user): the same device may sign into several
/// accounts, and each gets its own one-time import.
const MARKER_FILE: &str = "local-import.json";

/// Serializes every marker read-modify-write in this process. Two runtimes in
/// one process (a swap mid-flight, tests) must not lose an account's entry to
/// a racing update; cross-process exclusion is the data-dir `InstanceLock`'s job.
fn marker_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Marker {
    #[serde(default)]
    imports: Vec<MarkerEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkerEntry {
    org_id: String,
    user_id: String,
    imported_at_ms: i64,
    imported_chats: usize,
    imported_spaces: usize,
}

/// What the wizard needs to offer (or silently skip) the import step.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImportStatus {
    /// Chats in the local profile with no row in this synced workspace yet.
    pub available_chats: usize,
    /// Spaces in the local profile with no row in this synced workspace yet.
    pub available_spaces: usize,
    /// A completed import for this (org, user) is already on record.
    pub imported_before: bool,
}

/// Per-item progress for the wizard's progress step.
/// NB: `rename_all` renames the *variants* (the `kind` tag); struct-variant
/// *fields* need `rename_all_fields` — without it the wire shape silently
/// ships snake_case counters (caught by `import_events_serialize_camel_case`).
#[derive(Debug, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum ImportEvent {
    /// Emitted once, before any copying.
    Start { chats: usize, spaces: usize },
    /// One imported chat (docs + journal + row).
    Chat {
        index: usize,
        total: usize,
        chat_id: String,
        title: Option<String>,
    },
    /// Terminal summary — also the RPC's final stream item.
    Summary {
        imported_chats: usize,
        imported_spaces: usize,
        skipped_chats: usize,
        skipped_spaces: usize,
        journals_copied: usize,
        ledger_rows_merged: usize,
        errors: Vec<String>,
    },
}

/// The import service a synced runtime carries (`EngineCore::assemble` builds
/// one when the profile is account-scoped and a local profile exists on disk).
#[derive(Clone)]
pub struct LocalImporter {
    inner: Arc<ImporterInner>,
}

struct ImporterInner {
    data_dir: PathBuf,
    device_id: String,
    org_id: String,
    user_id: String,
    target_store: Arc<DocsStore>,
    target_journals: PathBuf,
    workspace: WorkspaceHost,
    uploads: Uploads,
}

impl LocalImporter {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        data_dir: &Path,
        device_id: &str,
        org_id: &str,
        user_id: &str,
        target_store: Arc<DocsStore>,
        target_journals: PathBuf,
        workspace: WorkspaceHost,
        uploads: Uploads,
    ) -> Self {
        Self {
            inner: Arc::new(ImporterInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                target_store,
                target_journals,
                workspace,
                uploads,
            }),
        }
    }

    fn source_root(&self) -> PathBuf {
        self.inner.data_dir.join("profiles").join("local")
    }

    fn source_uploads(&self) -> PathBuf {
        self.source_root().join("uploads")
    }

    fn marker_path(&self) -> PathBuf {
        self.inner.data_dir.join(MARKER_FILE)
    }

    /// Load the marker under [`marker_lock`]. A file that exists but does not
    /// parse is moved aside to `local-import.json.corrupt` — its grants were
    /// already unreadable (boot's [`marker_grants_read_root`] parses the same
    /// file), and keeping the bytes as evidence beats silently clobbering them
    /// on the next write.
    fn load_marker_locked(&self) -> Marker {
        let path = self.marker_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => return Marker::default(),
        };
        match serde_json::from_str(&raw) {
            Ok(marker) => marker,
            Err(err) => {
                let aside = path.with_extension("json.corrupt");
                tracing::warn!(error = %err, aside = %aside.display(),
                    "local-import marker is corrupt; moving it aside");
                if let Err(err) = std::fs::rename(&path, &aside) {
                    tracing::warn!(error = %err, "corrupt marker aside failed");
                }
                Marker::default()
            }
        }
    }

    fn imported_before(&self) -> bool {
        let _guard = marker_lock();
        self.load_marker_locked()
            .imports
            .iter()
            .any(|e| e.org_id == self.inner.org_id && e.user_id == self.inner.user_id)
    }

    /// Record a completed import and re-arm the read-only uploads root for
    /// future boots (`EngineCore::assemble` consults [`marker_grants_read_root`]).
    ///
    /// The read-modify-write is serialized process-wide (cross-process is
    /// already excluded by the data-dir `InstanceLock`) and published
    /// atomically — write + fsync a sibling temp file, then rename over the
    /// marker — so a crash or short write can never truncate the file and
    /// erase other accounts' grants. Failures propagate to the caller and end
    /// up in the import summary.
    fn record_import(&self, chats: usize, spaces: usize) -> Result<(), EngineError> {
        let _guard = marker_lock();
        let mut marker = self.load_marker_locked();
        marker
            .imports
            .retain(|e| !(e.org_id == self.inner.org_id && e.user_id == self.inner.user_id));
        marker.imports.push(MarkerEntry {
            org_id: self.inner.org_id.clone(),
            user_id: self.inner.user_id.clone(),
            imported_at_ms: crate::now_ms(),
            imported_chats: chats,
            imported_spaces: spaces,
        });
        let bytes = serde_json::to_vec_pretty(&marker)
            .map_err(|err| EngineError::Other(format!("marker serialize: {err}")))?;
        let path = self.marker_path();
        let tmp = path.with_extension("json.tmp");
        let publish = || -> std::io::Result<()> {
            {
                use std::io::Write;
                let mut file = std::fs::File::create(&tmp)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            std::fs::rename(&tmp, &path)
        };
        publish().map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            EngineError::Other(format!("marker write: {err}"))
        })
    }

    /// Open the local profile's stores read-only-ish. `None` when the device
    /// never ran a local profile (nothing to import).
    fn open_source(&self) -> Result<Option<(DocsStore, RegistryDoc)>, EngineError> {
        let root = self.source_root();
        if !root.join("docs.sqlite3").is_file() {
            return Ok(None);
        }
        let store = DocsStore::open(&root)?;
        let Some(bytes) = store.load_snapshot(REGISTRY_DOC_ID)? else {
            return Ok(None); // store exists but no registry replica — nothing rowed
        };
        let registry = RegistryDoc::from_bytes(&bytes, &self.inner.device_id)?;
        Ok(Some((store, registry)))
    }

    /// What's importable right now (target-dedup applied).
    pub fn status(&self) -> Result<LocalImportStatus, EngineError> {
        let imported_before = self.imported_before();
        let Some((_, registry)) = self.open_source()? else {
            return Ok(LocalImportStatus {
                available_chats: 0,
                available_spaces: 0,
                imported_before,
            });
        };
        let mut available_chats = 0;
        for chat in registry.read_chats()? {
            if self.inner.workspace.chat(&chat.id)?.is_none() {
                available_chats += 1;
            }
        }
        let mut available_spaces = 0;
        for space in registry.read_spaces()? {
            if self.inner.workspace.space(&space.id)?.is_none() {
                available_spaces += 1;
            }
        }
        Ok(LocalImportStatus {
            available_chats,
            available_spaces,
            imported_before,
        })
    }

    /// Run the import, emitting [`ImportEvent`]s (the last is always
    /// `Summary`). Blocking (sqlite + fs) — callers run it off the async path.
    pub fn run(&self, mut emit: impl FnMut(ImportEvent)) -> Result<(), EngineError> {
        let Some((source_store, registry)) = self.open_source()? else {
            emit(ImportEvent::Start {
                chats: 0,
                spaces: 0,
            });
            emit(ImportEvent::Summary {
                imported_chats: 0,
                imported_spaces: 0,
                skipped_chats: 0,
                skipped_spaces: 0,
                journals_copied: 0,
                ledger_rows_merged: 0,
                errors: Vec::new(),
            });
            return Ok(());
        };

        let mut errors: Vec<String> = Vec::new();

        // Spaces first: chats reference `space_id`, and viewers resolve the
        // reference as soon as the chat row lands.
        let spaces = registry.read_spaces()?;
        let chats = registry.read_chats()?;
        let (total_chats, total_spaces) = (chats.len(), spaces.len());
        let pending_chats: Vec<_> = chats
            .into_iter()
            .filter(|chat| !matches!(self.inner.workspace.chat(&chat.id), Ok(Some(_))))
            .collect();
        let pending_spaces: Vec<_> = spaces
            .into_iter()
            .filter(|space| !matches!(self.inner.workspace.space(&space.id), Ok(Some(_))))
            .collect();
        let skipped_chats = total_chats - pending_chats.len();
        let skipped_spaces = total_spaces - pending_spaces.len();

        emit(ImportEvent::Start {
            chats: pending_chats.len(),
            spaces: pending_spaces.len(),
        });

        let mut imported_spaces = 0;
        for space in &pending_spaces {
            match self.inner.workspace.import_space_row(space) {
                Ok(()) => imported_spaces += 1,
                Err(err) => errors.push(format!("space {}: {err}", space.id)),
            }
        }

        let source_journals = self.source_root().join("journals");
        let total = pending_chats.len();
        let mut imported_chats = 0;
        let mut journals_copied = 0;
        for (index, chat) in pending_chats.iter().enumerate() {
            emit(ImportEvent::Chat {
                index,
                total,
                chat_id: chat.id.clone(),
                title: chat.title.clone(),
            });
            match self.import_chat(&source_store, chat, &source_journals) {
                Ok(journal) => {
                    imported_chats += 1;
                    if journal {
                        journals_copied += 1;
                    }
                }
                Err(err) => errors.push(format!("chat {}: {err}", chat.id)),
            }
        }

        // Merge the source's command ledger so imported pending commands can
        // never re-execute under this profile (mark-before-execute carries over).
        let ledger_rows_merged = source_store
            .processed_commands()
            .and_then(|rows| self.inner.target_store.import_processed_commands(&rows))
            .unwrap_or_else(|err| {
                errors.push(format!("command ledger: {err}"));
                0
            });

        // Transcripts embed absolute paths under the local uploads root; jail
        // it read-only now (and on future boots, via the marker). A marker
        // that fails to persist means imported attachments stop resolving
        // after a restart — that is a real failure, not a footnote.
        if self.source_uploads().is_dir() {
            self.inner
                .uploads
                .add_read_only_root(&self.source_uploads());
        }
        if let Err(err) = self.record_import(imported_chats, imported_spaces) {
            errors.push(format!("import marker: {err}"));
        }

        emit(ImportEvent::Summary {
            imported_chats,
            imported_spaces,
            skipped_chats,
            skipped_spaces,
            journals_copied,
            ledger_rows_merged,
            errors,
        });
        Ok(())
    }

    /// Copy one chat: doc snapshot (born-chat2 shape), journal files, row.
    /// Returns whether a journal file was copied.
    fn import_chat(
        &self,
        source_store: &DocsStore,
        chat: &zeron_proto::Chat,
        source_journals: &Path,
    ) -> Result<bool, EngineError> {
        // Doc bytes may be absent (a chat row created but never opened) — the
        // row alone is still worth carrying; a doc materializes on first open.
        if let Some(bytes) = source_store.load_snapshot(&chat.id)?
            && !self.inner.target_store.has_snapshot(&chat.id)?
        {
            self.inner.target_store.save_snapshot_with_cursor(
                &chat.id,
                &bytes,
                0,
                CHAT2_DOC_EPOCH,
            )?;
        }

        let mut copied = false;
        let (src_journal, src_resume) = journal_paths(source_journals, &chat.id);
        let (dst_journal, dst_resume) = journal_paths(&self.inner.target_journals, &chat.id);
        if src_journal.is_file() && !dst_journal.exists() {
            std::fs::create_dir_all(&self.inner.target_journals)?;
            std::fs::copy(&src_journal, &dst_journal)?;
            copied = true;
        }
        if src_resume.is_file() && !dst_resume.exists() {
            std::fs::create_dir_all(&self.inner.target_journals)?;
            std::fs::copy(&src_resume, &dst_resume)?;
        }

        // Row last: once it appears in watchers the chat is clickable, and by
        // then its doc + journal are already in place. Chat2 lineage is
        // explicit so `DocHost::open` never routes the import down the legacy
        // s2 adopt branch.
        let mut row = chat.clone();
        row.room_gen = Some(CHAT2_DOC_EPOCH);
        self.inner.workspace.import_chat_row(&row)?;
        Ok(copied)
    }
}

#[cfg(test)]
mod tests {
    use super::ImportEvent;

    #[test]
    fn import_events_serialize_camel_case() {
        let summary = serde_json::to_value(ImportEvent::Summary {
            imported_chats: 2,
            imported_spaces: 1,
            skipped_chats: 0,
            skipped_spaces: 0,
            journals_copied: 2,
            ledger_rows_merged: 3,
            errors: Vec::new(),
        })
        .expect("serialize");
        assert_eq!(summary["kind"], "summary");
        assert_eq!(
            summary["importedChats"], 2,
            "fields must be camelCase: {summary}"
        );
        let chat = serde_json::to_value(ImportEvent::Chat {
            index: 0,
            total: 2,
            chat_id: "c1".into(),
            title: None,
        })
        .expect("serialize");
        assert_eq!(chat["chatId"], "c1", "fields must be camelCase: {chat}");
    }
}

/// Whether a recorded import grants the synced profile `(org, user)` the local
/// profile's uploads root as a read-only jail root. `EngineCore::assemble`
/// calls this on every account-scoped boot.
pub fn marker_grants_read_root(data_dir: &Path, org_id: &str, user_id: &str) -> Option<PathBuf> {
    let marker: Marker = std::fs::read_to_string(data_dir.join(MARKER_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())?;
    let hit = marker
        .imports
        .iter()
        .any(|e| e.org_id == org_id && e.user_id == user_id);
    let uploads = data_dir.join("profiles").join("local").join("uploads");
    (hit && uploads.is_dir()).then_some(uploads)
}
