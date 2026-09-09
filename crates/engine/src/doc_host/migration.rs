//! Copy retained history into encrypted rooms. Never deletes the source.
use super::*;
use loro::ToJson;
use serde::{Deserialize, Serialize};
use zeron_crypto::content::{ContentPurpose, MAX_PLAINTEXT_BYTES};
use zeron_sync::chat_frames::{self as wire, frame_type};

const JOURNAL: &str = "__encrypted_history_migration_v1";
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStatus {
    pub phase: String,
    pub total: usize,
    pub completed: usize,
    pub error: Option<String>,
    pub legacy_retained: bool,
}

pub struct EncryptionPreparation(DocHost);
impl Drop for EncryptionPreparation {
    fn drop(&mut self) {
        self.0.finish_encryption_preparation();
    }
}

fn fail(error: impl std::fmt::Display) -> EngineError {
    EngineError::Other(error.to_string())
}

impl DocHost {
    pub(super) fn history_ready_for_commands(&self, chat: &str) -> bool {
        if !self.vault().is_some_and(|v| v.is_enrolled()) {
            return true;
        }
        let check = || -> Result<bool, EngineError> {
            if self
                .inner
                .store
                .has_snapshot(&format!("__encrypted_history_seed:{chat}"))?
            {
                return Ok(true);
            }
            if self
                .inner
                .store
                .has_snapshot(&format!("{JOURNAL}:source:{chat}"))?
            {
                return Ok(false);
            }
            let required: HashSet<String> = self
                .inner
                .store
                .load_snapshot("__plaintext_history_inventory")?
                .map(|bytes| serde_json::from_slice(&bytes))
                .transpose()
                .map_err(fail)?
                .unwrap_or_default();
            if required.contains(chat) {
                return Ok(false);
            }
            Ok(!self
                .inner
                .store
                .load_snapshot_with_cursor(chat)?
                .is_some_and(|(_, _, epoch)| epoch < crate::chat2_host::CHAT2_ENCRYPTED_DOC_EPOCH))
        };
        check().unwrap_or(false)
    }

    pub fn history_migration_status(&self) -> MigrationStatus {
        lock(&self.inner.migration_status).clone()
    }

    pub fn prepare_encryption(&self) -> Result<EncryptionPreparation, EngineError> {
        if lock(&self.inner.handles)
            .values()
            .any(|h| Arc::strong_count(&h.doc) > 1)
        {
            return Err(fail("Finish running sessions before enabling encryption."));
        }
        if self.workspace().is_some_and(|ws| !ws.connected()) {
            return Err(fail("Wait for workspace sync before enabling encryption."));
        }
        if let Some(workspace) = self.workspace() {
            let ids: HashSet<String> = workspace
                .watch_chats()
                .borrow()
                .iter()
                .map(|c| c.id.clone())
                .collect();
            self.inner.store.save_snapshot(
                "__plaintext_history_inventory",
                &serde_json::to_vec(&ids).map_err(fail)?,
            )?;
        }
        self.inner
            .encryption_preparing
            .store(true, Ordering::Release);
        let guard = EncryptionPreparation(self.clone());
        self.retire_plaintext_handles()?;
        Ok(guard)
    }

    fn retire_plaintext_handles(&self) -> Result<(), EngineError> {
        let handles: Vec<_> = lock(&self.inner.handles)
            .values()
            .filter(|h| !h.encrypted)
            .cloned()
            .collect();
        for handle in handles {
            self.inner
                .store
                .save_snapshot(&handle.chat_id, &handle.doc.export_snapshot()?)?;
            handle.retired.store(true, Ordering::Release);
            lock(&handle.chat2).take();
            lock(&handle.chat2_local_sub).take();
            lock(&self.inner.handles).remove(&handle.chat_id);
        }
        Ok(())
    }

    pub fn finish_encryption_preparation(&self) {
        let _ = self.retire_plaintext_handles();
        self.inner
            .encryption_preparing
            .store(false, Ordering::Release);
        self.kick_drains();
    }

    pub fn start_history_migration(&self) {
        if self.inner.migration_running.load(Ordering::Acquire) {
            return;
        }
        let host = self.clone();
        self.spawn_worker(async move {
            let _ = host.migrate_history().await;
        });
    }

    pub(super) fn spawn_encrypted_history_migration(&self) {
        let host = self.clone();
        self.spawn_worker(async move {
            loop {
                if host.vault().is_some_and(|v| v.is_ready()) {
                    let _ = host.migrate_history().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    /// Explicit retry uses the same durable journal as automatic migration.
    pub async fn migrate_history(&self) -> Result<(), EngineError> {
        let _migration = self.inner.migration_lock.lock().await;
        self.inner.migration_running.store(true, Ordering::Release);
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _running = Running(&self.inner.migration_running);
        let result = self.migrate_history_inner().await;
        self.inner.migration_running.store(false, Ordering::Release);
        if let Err(error) = &result {
            let mut status = lock(&self.inner.migration_status);
            status.phase = "paused".into();
            status.error = Some(error.to_string());
        }
        self.kick_drains();
        result
    }

    async fn migrate_history_inner(&self) -> Result<(), EngineError> {
        let vault = self
            .vault()
            .filter(|v| v.is_ready())
            .ok_or_else(|| fail("Vault is not ready"))?;
        let edge = self
            .inner
            .config
            .edge
            .as_ref()
            .ok_or_else(|| fail("No sync server"))?;
        let workspace = self
            .workspace()
            .ok_or_else(|| fail("Workspace is not ready"))?;
        let chats = workspace.watch_chats().borrow().clone();
        let key = format!(
            "{JOURNAL}:{}",
            vault.status().genesis_hash.unwrap_or_default()
        );
        let mut done: HashSet<String> = self
            .inner
            .store
            .load_snapshot(&key)?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(fail)?
            .unwrap_or_default();
        *lock(&self.inner.migration_status) = MigrationStatus {
            phase: "copying".into(),
            total: chats.len(),
            completed: chats.iter().filter(|c| done.contains(&c.id)).count(),
            error: None,
            legacy_retained: true,
        };
        let mut first_error = None;
        for chat in chats {
            if done.contains(&chat.id) {
                continue;
            }
            if !vault.is_ready() {
                return Err(fail("Waiting for encryption keys"));
            }
            if let Err(error) = self
                .migrate_chat_history(&chat.id, chat.room_gen.unwrap_or(1), edge, vault)
                .await
            {
                first_error.get_or_insert_with(|| fail(format!("Chat {}: {error}", chat.id)));
                continue;
            }
            done.insert(chat.id);
            self.inner
                .store
                .save_snapshot(&key, &serde_json::to_vec(&done).map_err(fail)?)?;
            lock(&self.inner.migration_status).completed += 1;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let mut status = lock(&self.inner.migration_status);
        status.phase = "complete".into();
        status.error = None;
        Ok(())
    }

    async fn migrate_chat_history(
        &self,
        chat: &str,
        generation: u32,
        edge: &EdgeConfig,
        vault: &crate::vault::VaultService,
    ) -> Result<(), EngineError> {
        if chat.is_empty()
            || chat.len() > 125
            || !chat
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(fail("Invalid legacy chat identifier"));
        }
        let source = loro::LoroDoc::new();
        let stored = self.inner.store.load_snapshot_with_cursor(chat)?;
        // Keep immutable local source snapshots separate from the live encrypted doc.
        let backup_key = format!("{JOURNAL}:source:{chat}");
        if let Some(bytes) = self.inner.store.load_snapshot(&backup_key)? {
            source.import(&bytes).map_err(fail)?;
        }
        if let Some((bytes, _, epoch)) = &stored {
            if *epoch < crate::chat2_host::CHAT2_ENCRYPTED_DOC_EPOCH {
                source.import(bytes).map_err(fail)?;
            }
        }
        let cached = lock(&self.inner.handles).get(chat).cloned();
        if let Some(handle) = &cached {
            if !handle.encrypted {
                if Arc::strong_count(&handle.doc) > 1 {
                    return Err(fail("Waiting for the running session to finish"));
                }
                source
                    .import(&handle.doc.export_snapshot()?)
                    .map_err(fail)?;
                self.save_snapshot(handle);
                handle.retired.store(true, Ordering::Release);
                lock(&handle.chat2).take();
                lock(&handle.chat2_local_sub).take();
                lock(&self.inner.handles).remove(chat);
            }
        }

        let retained_generation = self
            .inner
            .store
            .load_snapshot(&format!("{backup_key}:generation"))?;
        let generation = if let Some(bytes) = retained_generation {
            u32::from_le_bytes(
                bytes
                    .try_into()
                    .map_err(|_| fail("Invalid retained history generation"))?,
            )
        } else if stored
            .as_ref()
            .is_some_and(|(_, _, epoch)| *epoch >= crate::chat2_host::CHAT2_DOC_EPOCH)
        {
            generation.max(2)
        } else {
            generation
        };
        if generation < 2 {
            if let Some(bytes) = self
                .migration_get(edge, &format!("/snapshot/{chat}"), true)
                .await?
            {
                if !bytes.is_empty() {
                    source.import(&bytes).map_err(fail)?;
                }
            }
        } else {
            self.import_migration_room(edge, chat, &source, None)
                .await?;
        }
        if source.oplog_vv().is_empty() {
            let required: HashSet<String> = self
                .inner
                .store
                .load_snapshot("__plaintext_history_inventory")?
                .map(|bytes| serde_json::from_slice(&bytes))
                .transpose()
                .map_err(fail)?
                .unwrap_or_default();
            if !required.contains(chat) {
                if stored.as_ref().is_some_and(|(_, _, epoch)| {
                    *epoch >= crate::chat2_host::CHAT2_ENCRYPTED_DOC_EPOCH
                }) {
                    return Ok(());
                }
                let codec = crate::chat2_host::ChatCodec::new(vault.clone(), chat);
                let existing = loro::LoroDoc::new();
                if self
                    .import_migration_room(
                        edge,
                        &crate::chat2_host::encrypted_room_id(chat),
                        &existing,
                        Some(&codec),
                    )
                    .await?
                    .1
                {
                    return Ok(());
                }
            }
            return Err(fail(
                "History is unavailable; bring its original device online and retry",
            ));
        }
        let snapshot = source.export(loro::ExportMode::Snapshot).map_err(fail)?;
        self.inner.store.save_snapshot(&backup_key, &snapshot)?;
        let source_frontier = source.oplog_vv();
        let source_json = source.get_deep_value().to_json_value();
        let mut refs = HashSet::new();
        collect_blob_refs(&source_json, &mut refs);
        let codec = crate::chat2_host::ChatCodec::new(vault.clone(), chat);
        for reference in refs {
            let (owner, part) = reference
                .split_once('/')
                .ok_or_else(|| fail("Invalid sidecar reference"))?;
            if owner != chat
                || part.is_empty()
                || part == "."
                || part == ".."
                || part.len() > 200
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:#~-".contains(&b))
            {
                return Err(fail("Invalid sidecar reference"));
            }
            let path = format!("/blob/{chat}/{}", encode_part_segment(part));
            let bytes = self
                .migration_get(edge, &path, true)
                .await?
                .ok_or_else(|| {
                    fail("Full tool output is unavailable. Open the original device and retry.")
                })?;
            // Existing branch builds may already have sealed this old path.
            let plaintext =
                if zeron_crypto::record::UnverifiedRecord::parse(&bytes, MAX_SIDECAR_PLAINTEXT)
                    .is_ok()
                {
                    codec
                        .open_async(ContentPurpose::Blob, &bytes, MAX_SIDECAR_PLAINTEXT)
                        .await
                        .map_err(|e| fail(format!("Blob verification failed: {e:?}")))?
                } else {
                    bytes
                };
            std::str::from_utf8(&plaintext).map_err(|_| {
                fail("Tool output is not valid text; retained the source for review")
            })?;
            if part.ends_with(".diff") {
                serde_json::from_slice::<serde_json::Value>(&plaintext).map_err(fail)?;
            }
            let target = format!(
                "/blob/{}/{}",
                crate::chat2_host::encrypted_room_id(chat),
                encode_part_segment(part)
            );
            self.copy_migration_content(edge, &codec, &target, ContentPurpose::Blob, &plaintext)
                .await?;
        }
        let room = crate::chat2_host::encrypted_room_id(chat);
        let target = loro::LoroDoc::new();
        let (cursor, _, expected_frontier) = self
            .import_migration_room(edge, &room, &target, Some(&codec))
            .await?;
        target.import(&snapshot).map_err(fail)?;
        // Merge any local encrypted work, including pending commands, without
        // rebuilding IDs or touching the processed-command ledger.
        if let Some(handle) = lock(&self.inner.handles).get(chat).cloned() {
            target
                .import(&handle.doc.export_snapshot()?)
                .map_err(fail)?;
        } else if let Some((bytes, _, epoch)) = &stored {
            if *epoch >= crate::chat2_host::CHAT2_ENCRYPTED_DOC_EPOCH {
                target.import(bytes).map_err(fail)?;
            }
        }
        let full = target.export(loro::ExportMode::Snapshot).map_err(fail)?;
        let frontier = target.oplog_vv().encode();
        let sealed_frontier = codec
            .seal(ContentPurpose::Frontier, &frontier, 64 * 1024)
            .await?;
        let sealed = codec
            .seal(ContentPurpose::Checkpoint, &full, MAX_PLAINTEXT_BYTES)
            .await?;
        let bearer = edge.bearer().await.ok_or_else(|| fail("Signed out"))?;
        let response = self
            .inner
            .http
            .post(format!(
                "{}/chat2/{room}/checkpoint?seqCovered={cursor}&refreshReaders=1",
                edge.url.trim_end_matches('/')
            ))
            .bearer_auth(bearer)
            .header(
                "x-chat2-expected-frontier",
                base64::engine::general_purpose::STANDARD.encode(expected_frontier),
            )
            .header(
                "x-chat2-frontier",
                base64::engine::general_purpose::STANDARD.encode(sealed_frontier.encoded()),
            )
            .body(sealed.encoded().to_vec())
            .send()
            .await
            .map_err(fail)?;
        if !response.status().is_success() {
            return Err(fail(format!("Checkpoint upload: {}", response.status())));
        }
        let verified = loro::LoroDoc::new();
        self.import_migration_room(edge, &room, &verified, Some(&codec))
            .await?;
        if !verified.oplog_vv().includes_vv(&source_frontier) {
            return Err(fail("Encrypted copy does not cover the source history"));
        }
        let view = SessionDoc::from_doc(verified);
        let tail = zeron_doc::materialize_tail(&view, now_ms(), 64)?;
        let tail = serde_json::to_vec(&tail).map_err(fail)?;
        self.copy_migration_content(
            edge,
            &codec,
            &format!("/chat2/{room}/tail"),
            ContentPurpose::Tail,
            &tail,
        )
        .await?;
        let cached = lock(&self.inner.handles).get(chat).cloned();
        if let Some(handle) = cached {
            handle.doc.doc().import(&full).map_err(fail)?;
            self.inner
                .store
                .save_snapshot(chat, &handle.doc.export_snapshot()?)?;
            handle.publish_messages();
        } else {
            self.inner.store.save_snapshot_with_cursor(
                chat,
                &full,
                cursor,
                crate::chat2_host::CHAT2_ENCRYPTED_DOC_EPOCH,
            )?;
        }
        self.inner
            .store
            .save_snapshot(&format!("__encrypted_history_seed:{chat}"), b"verified")?;
        Ok(())
    }

    async fn migration_get(
        &self,
        edge: &EdgeConfig,
        path: &str,
        optional: bool,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let bearer = edge.bearer().await.ok_or_else(|| fail("Signed out"))?;
        let mut response = self
            .inner
            .http
            .get(format!("{}{}", edge.url.trim_end_matches('/'), path))
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(fail)?;
        if optional && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(fail(format!("History download: {}", response.status())));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_DOWNLOAD as u64)
        {
            return Err(fail("History object exceeds the migration size limit"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(fail)? {
            if bytes.len() + chunk.len() > MAX_DOWNLOAD {
                return Err(fail("History object exceeds the migration size limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }

    async fn import_migration_room(
        &self,
        edge: &EdgeConfig,
        room: &str,
        doc: &loro::LoroDoc,
        codec: Option<&crate::chat2_host::ChatCodec>,
    ) -> Result<(u64, bool, Vec<u8>), EngineError> {
        let mut cursor = 0;
        let mut checkpoint_loaded = false;
        let mut present = false;
        let mut checkpoint_frontier: Option<Vec<u8>> = None;
        for _ in 0..10_000 {
            let bytes = self
                .migration_get(edge, &format!("/chat2/{room}/rows?after={cursor}"), false)
                .await?
                .unwrap();
            let frames = decode_migration_frames(&bytes)?;
            let first = frames
                .first()
                .ok_or_else(|| fail("Missing history state"))?;
            if first.kind != frame_type::STATE {
                return Err(fail("Invalid history state"));
            }
            if checkpoint_frontier
                .as_ref()
                .is_some_and(|previous| previous != &first.payload)
            {
                return Err(fail("History checkpoint changed; retry migration"));
            }
            checkpoint_frontier = Some(first.payload.clone());
            let state: wire::StateHeader =
                serde_json::from_value(first.header.clone()).map_err(fail)?;
            if state.checkpoint_seq > state.head_seq || state.seq_floor > state.head_seq {
                return Err(fail("Invalid history revision bounds"));
            }
            present |= state.checkpoint_size > 0 || state.head_seq > 0;
            if state.checkpoint_size > 0 && (!checkpoint_loaded || cursor < state.seq_floor) {
                let mut cp = self
                    .migration_get(edge, &format!("/chat2/{room}/checkpoint"), false)
                    .await?
                    .unwrap();
                if let Some(codec) = codec {
                    cp = codec
                        .open_async(ContentPurpose::Checkpoint, &cp, MAX_PLAINTEXT_BYTES)
                        .await
                        .map_err(|e| fail(format!("Checkpoint verification failed: {e:?}")))?;
                }
                doc.import(&cp).map_err(fail)?;
                checkpoint_loaded = true;
                // Re-fetch at the boundary and require the same checkpoint
                // frontier; concurrent replacements must restart this copy.
                cursor = state.checkpoint_seq;
                continue;
            }
            let before = cursor;
            let mut ended = false;
            for frame in frames.into_iter().skip(1) {
                match frame.kind {
                    frame_type::ROW => {
                        let seq = frame
                            .header
                            .get("seq")
                            .and_then(serde_json::Value::as_u64)
                            .ok_or_else(|| fail("Invalid history row"))?;
                        if seq <= cursor {
                            continue;
                        }
                        if seq != cursor + 1 {
                            return Err(fail("History has a sequence gap; retry migration"));
                        }
                        let payload = if let Some(codec) = codec {
                            codec
                                .open_async(
                                    ContentPurpose::ChatUpdate,
                                    &frame.payload,
                                    MAX_PLAINTEXT_BYTES,
                                )
                                .await
                                .map_err(|e| fail(format!("History verification failed: {e:?}")))?
                        } else {
                            frame.payload
                        };
                        doc.import(&payload).map_err(fail)?;
                        cursor = seq;
                    }
                    frame_type::ROWS_DONE => ended = true,
                    _ => return Err(fail("Unexpected history frame")),
                }
            }
            if ended && cursor >= state.head_seq {
                return Ok((cursor, present, checkpoint_frontier.unwrap_or_default()));
            }
            if cursor == before {
                return Err(fail("History download made no progress"));
            }
        }
        Err(fail("History pagination limit exceeded"))
    }

    async fn copy_migration_content(
        &self,
        edge: &EdgeConfig,
        codec: &crate::chat2_host::ChatCodec,
        path: &str,
        purpose: ContentPurpose,
        bytes: &[u8],
    ) -> Result<(), EngineError> {
        if purpose == ContentPurpose::Blob {
            if let Some(existing) = self.migration_get(edge, path, true).await? {
                let opened = codec
                    .open_async(purpose, &existing, MAX_SIDECAR_PLAINTEXT)
                    .await
                    .map_err(|e| fail(format!("Existing sidecar verification failed: {e:?}")))?;
                if opened == bytes {
                    return Ok(());
                }
                return Err(fail(
                    "Encrypted tool output differs from its legacy copy; preserved both for review",
                ));
            }
        }
        let sealed = codec.seal(purpose, bytes, MAX_SIDECAR_PLAINTEXT).await?;
        let bearer = edge.bearer().await.ok_or_else(|| fail("Signed out"))?;
        let response = self
            .inner
            .http
            .put(format!("{}{}", edge.url.trim_end_matches('/'), path))
            .bearer_auth(bearer)
            .header("content-type", "application/octet-stream")
            .body(sealed.encoded().to_vec())
            .send()
            .await
            .map_err(fail)?;
        if !response.status().is_success() {
            return Err(fail(format!("Sidecar upload: {}", response.status())));
        }
        let saved = self.migration_get(edge, path, false).await?.unwrap();
        let opened = codec
            .open_async(purpose, &saved, MAX_SIDECAR_PLAINTEXT)
            .await
            .map_err(|e| fail(format!("Sidecar verification failed: {e:?}")))?;
        if opened != bytes {
            return Err(fail("Sidecar read-back mismatch"));
        }
        Ok(())
    }
}

fn decode_migration_frames(bytes: &[u8]) -> Result<Vec<wire::WireFrame>, EngineError> {
    let mut offset = 0;
    let mut frames = Vec::new();
    while offset < bytes.len() {
        let length = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| fail("Truncated history frame"))?;
        let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
        offset += 4;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| fail("Oversized history frame"))?;
        let frame = wire::decode(
            bytes
                .get(offset..end)
                .ok_or_else(|| fail("Truncated history frame"))?,
        )
        .ok_or_else(|| fail("Invalid history frame"))?;
        if frames.len() >= 50_000 {
            return Err(fail("Too many history frames"));
        }
        frames.push(frame);
        offset = end;
    }
    Ok(frames)
}

fn collect_blob_refs(value: &serde_json::Value, refs: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "outputRef" | "diffRef") {
                    if let Some(reference) = value.as_str() {
                        refs.insert(reference.to_owned());
                    }
                } else {
                    collect_blob_refs(value, refs);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_blob_refs(value, refs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_or_malformed_history_streams_are_rejected() {
        let frame = wire::encode(frame_type::STATE, &serde_json::json!({"headSeq": 0}), &[]);
        let mut stream = (frame.len() as u32).to_le_bytes().to_vec();
        stream.extend(&frame);
        assert_eq!(decode_migration_frames(&stream).unwrap().len(), 1);
        for end in 1..stream.len() {
            assert!(decode_migration_frames(&stream[..end]).is_err());
        }
        stream.extend([1, 0]);
        assert!(decode_migration_frames(&stream).is_err());
    }
}
