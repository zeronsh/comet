//! Inputs to the shared transcript. Document feeds contain the real document
//! entries; they do not own an engine, transport, or a second message schema.

use gpui::{App, Context, Entity};
use zeron_doc::SessionMessageEntry;

use super::TranscriptReplayState;
use crate::state::AppState;

/// A host-independent, already-replayed document for presentation.
///
/// Mutate through `update_entries` or `replace` so observers see a new revision.
/// The document id also namespaces locally seeded attachment-cache entries.
pub struct TranscriptDocument {
    doc_id: String,
    entries: Vec<SessionMessageEntry>,
    revision: u64,
}

impl TranscriptDocument {
    pub fn new(doc_id: impl Into<String>, entries: Vec<SessionMessageEntry>) -> Self {
        Self {
            doc_id: doc_id.into(),
            entries,
            revision: 0,
        }
    }

    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }
    pub fn entries(&self) -> &[SessionMessageEntry] {
        &self.entries
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace a snapshot, including an empty reset or a switch to another doc.
    pub fn replace(
        &mut self,
        doc_id: impl Into<String>,
        entries: Vec<SessionMessageEntry>,
        cx: &mut Context<Self>,
    ) {
        self.doc_id = doc_id.into();
        self.entries = entries;
        self.changed(cx);
    }

    /// Apply a local update without cloning the document's text/tool payloads.
    pub fn update_entries(
        &mut self,
        update: impl FnOnce(&mut Vec<SessionMessageEntry>),
        cx: &mut Context<Self>,
    ) {
        update(&mut self.entries);
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.revision = self.revision.wrapping_add(1);
        cx.notify();
    }
}

#[derive(Clone)]
pub(super) enum TranscriptSource {
    Native(Entity<AppState>),
    Document(Entity<TranscriptDocument>),
}

/// Borrowed for row derivation or rail marks, never a cloned transcript frame.
pub(super) struct TranscriptSnapshot<'a> {
    pub doc_id: Option<&'a str>,
    pub entries: &'a [SessionMessageEntry],
    pub echoes: &'a [SessionMessageEntry],
    pub replay: TranscriptReplayState,
    pub revision: u64,
}

impl TranscriptSource {
    pub fn snapshot<'a>(
        &'a self,
        doc_override: Option<&'a str>,
        cx: &'a App,
    ) -> TranscriptSnapshot<'a> {
        match self {
            Self::Native(entity) => {
                let state = entity.read(cx);
                let (doc_id, entries, echoes, replay) = match doc_override {
                    Some(doc_id) => (
                        Some(doc_id),
                        state.sub_transcript(doc_id),
                        &[][..],
                        TranscriptReplayState::Populated,
                    ),
                    None => (
                        state.selected_chat.as_deref(),
                        state.transcript.as_slice(),
                        state.pending_echoes(),
                        if !state.transcript_replayed {
                            TranscriptReplayState::Pending
                        } else if state.transcript.is_empty() {
                            TranscriptReplayState::Empty
                        } else {
                            TranscriptReplayState::Populated
                        },
                    ),
                };
                TranscriptSnapshot {
                    doc_id,
                    entries,
                    echoes,
                    replay,
                    revision: state.transcript_revision,
                }
            }
            Self::Document(entity) => {
                let document = entity.read(cx);
                TranscriptSnapshot {
                    doc_id: Some(&document.doc_id),
                    entries: &document.entries,
                    echoes: &[],
                    replay: if document.entries.is_empty() {
                        TranscriptReplayState::Empty
                    } else {
                        TranscriptReplayState::Populated
                    },
                    revision: document.revision,
                }
            }
        }
    }

    pub fn is_document(&self) -> bool {
        matches!(self, Self::Document(_))
    }

    pub fn native(&self) -> Option<&Entity<AppState>> {
        match self {
            Self::Native(state) => Some(state),
            Self::Document(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    #[gpui::test]
    fn native_snapshot_preserves_primary_and_override_replay(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        let source = TranscriptSource::Native(state.clone());
        cx.update(|cx| {
            let primary = source.snapshot(None, cx);
            assert_eq!(primary.replay, TranscriptReplayState::Pending);
            assert_eq!(primary.entries.as_ptr(), state.read(cx).transcript.as_ptr());
            let child = source.snapshot(Some("child"), cx);
            assert_eq!(child.doc_id, Some("child"));
            // Existing native for_doc treats even an empty replay as populated.
            assert_eq!(child.replay, TranscriptReplayState::Populated);
            assert!(child.echoes.is_empty());
            assert_eq!(
                child.entries.as_ptr(),
                state.read(cx).sub_transcript("child").as_ptr()
            );
        });
    }

    #[gpui::test]
    fn document_updates_and_empty_resets_advance_revision(cx: &mut gpui::TestAppContext) {
        let document = cx.new(|_| TranscriptDocument::new("first", Vec::new()));
        document.update(cx, |document, cx| {
            assert_eq!(document.revision(), 0);
            document.update_entries(|entries| assert!(entries.is_empty()), cx);
            assert_eq!(document.revision(), 1);
            document.replace("second", Vec::new(), cx);
            assert_eq!(document.revision(), 2);
            assert_eq!(document.doc_id(), "second");
            assert!(document.entries().is_empty());
        });
    }
}
