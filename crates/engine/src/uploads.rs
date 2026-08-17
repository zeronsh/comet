//! Uploads — attachment staging on the chat's host device
//! (feature-inventory §3.7 "Uploads"; port of zeron's `uploads.ts`).
//!
//! The UI streams a file as base64 chunks (~60KB, sized for the relay when the
//! target device is remote); chunks stage on disk under `{uploads_root}/tmp/
//! {uploadId}/{seq}.b64` (surviving an engine restart mid-upload, unlike zeron's
//! in-memory buffers), and `commit` assembles them into
//! `{uploads_root}/{id8}-{name}` and returns the absolute path, which the
//! composer appends to the prompt so the agent can read the file from disk.
//! Attachments live only on the host device — every read proxies through the
//! owning device via `ReadAttachmentChunk`; nothing is mirrored to the edge.
//!
//! `read_chunk` serves transcript images back in 45KB base64 chunks. Path jail:
//! only files under the uploads dir or a workspace-known chat cwd are readable
//! (the RPC layer supplies the cwd roots) — and only supported image types, as
//! in zeron.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use crate::EngineError;

/// A pending upload must finish within this window (covers slow mesh links).
const STAGING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an assembled file.
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Multiple of 3 so independent base64 chunks concatenate losslessly.
const READ_CHUNK_BYTES: u64 = 45_000;

/// `ReadAttachmentChunk` reply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChunk {
    pub name: String,
    pub mime_type: String,
    /// Base64 of this chunk's byte range.
    pub data: String,
    pub next_offset: u64,
    pub done: bool,
}

struct UploadsInner {
    /// Profile-scoped durable home for new committed attachments.
    dir: PathBuf,
    /// Chunk staging (`{uploads_root}/tmp/{uploadId}/`).
    tmp: PathBuf,
    /// Historical roots accepted for reads only. Writes and staging never use
    /// them. RwLock: a local-profile import adds its source root at runtime so
    /// imported transcripts resolve without an engine restart.
    read_only_roots: std::sync::RwLock<Vec<PathBuf>>,
}

#[derive(Clone)]
pub struct Uploads {
    inner: Arc<UploadsInner>,
}

impl Uploads {
    /// Use the historical device-global uploads directory.
    pub fn new(data_dir: &Path) -> Self {
        Self::from_root(&data_dir.join("uploads"))
    }

    /// Use an already-resolved profile uploads directory.
    pub fn from_root(dir: &Path) -> Self {
        Self::from_root_with_fallback(dir, None)
    }

    /// Use a profile root for all writes and an optional legacy read-only root.
    pub fn from_root_with_fallback(dir: &Path, legacy_read_root: Option<&Path>) -> Self {
        Self {
            inner: Arc::new(UploadsInner {
                tmp: dir.join("tmp"),
                dir: dir.to_path_buf(),
                read_only_roots: std::sync::RwLock::new(
                    legacy_read_root
                        .into_iter()
                        .map(Path::to_path_buf)
                        .collect(),
                ),
            }),
        }
    }

    /// The durable uploads dir (a path-jail root).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Accept `root` for reads from now on (idempotent). Profile import calls
    /// this so transcripts that embed absolute paths under the local profile's
    /// uploads root keep resolving after the switch to a synced profile.
    pub fn add_read_only_root(&self, root: &Path) {
        let mut roots = self
            .inner
            .read_only_roots
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !roots.iter().any(|r| r == root) {
            roots.push(root.to_path_buf());
        }
    }

    /// Stage one base64 chunk. Positional (`seq`) writes are IDEMPOTENT: a client
    /// retrying a chunk whose ack was lost overwrites the same slot instead of
    /// double-appending. Callers without `seq` get append-only behavior.
    pub fn append(&self, upload_id: &str, data: &str, seq: Option<u64>) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = match seq {
            Some(seq) => seq,
            None => next_free_seq(&dir)?,
        };
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        // Base64 inflates by ~4/3; bound the staged payload against the file cap.
        let staged: u64 = chunk_files(&dir)?
            .iter()
            .filter(|(seq, _)| *seq != at)
            .map(|(_, path)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            .sum();
        if (staged + data.len() as u64) * 3 / 4 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.b64")), data)?;
        Ok(())
    }

    /// Assemble the staged chunks into a durable file and return its absolute
    /// path.
    pub fn commit(&self, upload_id: &str, file_name: &str) -> Result<String, EngineError> {
        let dir = self.staging_dir(upload_id)?;
        let mut parts = chunk_files(&dir)?;
        if parts.is_empty() {
            return Err(EngineError::Other("Unknown or expired upload".into()));
        }
        parts.sort_by_key(|(seq, _)| *seq);
        // Positional appends may leave holes if a chunk never arrived — joining
        // around them would silently corrupt the file.
        let mut joined = String::new();
        for (i, (seq, path)) in parts.iter().enumerate() {
            if *seq != i as u64 {
                return Err(EngineError::Other("Upload is missing a chunk".into()));
            }
            joined.push_str(std::fs::read_to_string(path)?.trim());
        }
        let bytes = BASE64
            .decode(joined.as_bytes())
            .map_err(|e| EngineError::Other(format!("upload is not valid base64: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::create_dir_all(&self.inner.dir)?;
        let name = sanitize(file_name);
        let id8: String = upload_id.chars().take(8).collect();
        let path = self.inner.dir.join(format!("{id8}-{name}"));
        std::fs::write(&path, &bytes)?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(path.to_string_lossy().to_string())
    }

    /// Read one 45KB chunk of an attachment. `extra_roots` are the workspace's
    /// known chat cwds — together with the uploads dir they form the path jail.
    pub fn read_chunk(
        &self,
        path: &str,
        offset: u64,
        extra_roots: &[PathBuf],
    ) -> Result<AttachmentChunk, EngineError> {
        use std::io::{Read, Seek};
        let file = self.inspect(path, extra_roots)?;
        let size = file.size;
        let start = offset.min(size);
        let next_offset = (start + READ_CHUNK_BYTES).min(size);
        // Read ONLY this chunk's byte range — never the whole file per chunk.
        let mut buf = vec![0u8; (next_offset - start) as usize];
        let mut handle = std::fs::File::open(&file.resolved)?;
        handle.seek(std::io::SeekFrom::Start(start))?;
        let mut read = 0usize;
        while read < buf.len() {
            let n = handle.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(AttachmentChunk {
            name: file.name,
            mime_type: file.mime_type,
            data: BASE64.encode(&buf),
            next_offset,
            done: next_offset >= size,
        })
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn staging_dir(&self, upload_id: &str) -> Result<PathBuf, EngineError> {
        // The id becomes a directory name — jail it to a safe charset.
        let ok = !upload_id.is_empty()
            && upload_id.len() <= 64
            && upload_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !ok {
            return Err(EngineError::Other("Invalid upload id".into()));
        }
        Ok(self.inner.tmp.join(upload_id))
    }

    /// Reclaim staging dirs whose newest chunk is older than the TTL (an upload
    /// abandoned mid-stream must not hold up to 32MB forever).
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.inner.tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let newest = std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false),
                None => true, // empty dir — reclaim
            };
            if expired {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn inspect(&self, path: &str, extra_roots: &[PathBuf]) -> Result<InspectedFile, EngineError> {
        let outside = || EngineError::Other("Attachment is outside the upload cache".into());
        // Canonicalize BOTH sides so `..` segments and symlinks can't escape.
        let resolved = std::fs::canonicalize(path).map_err(|_| outside())?;
        let read_roots = self
            .inner
            .read_only_roots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let allowed = std::iter::once(&self.inner.dir)
            .chain(read_roots.iter())
            .chain(extra_roots.iter())
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(&root) && resolved != root);
        if !allowed {
            return Err(outside());
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Err(EngineError::Other("Attachment is not a file".into()));
        }
        if meta.len() > MAX_BYTES {
            return Err(EngineError::Other("Attachment is too large".into()));
        }
        let mime_type = mime_by_ext(&resolved)
            .ok_or_else(|| EngineError::Other("Attachment is not a supported image".into()))?;
        Ok(InspectedFile {
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime_type: mime_type.to_string(),
            size: meta.len(),
            resolved,
        })
    }
}

struct InspectedFile {
    resolved: PathBuf,
    name: String,
    mime_type: String,
    size: u64,
}

fn chunk_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, EngineError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(seq) = seq
            && path.extension().and_then(|e| e.to_str()) == Some("b64")
        {
            files.push((seq, path));
        }
    }
    Ok(files)
}

fn next_free_seq(dir: &Path) -> Result<u64, EngineError> {
    Ok(chunk_files(dir)?
        .iter()
        .map(|(seq, _)| seq + 1)
        .max()
        .unwrap_or(0))
}

fn sanitize(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "upload".into()
    } else {
        tail
    }
}

fn mime_by_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("my photo (1).png"), "my_photo__1_.png");
        assert_eq!(sanitize(""), "upload");
    }

    #[test]
    fn commit_assembles_chunks_into_a_durable_file() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = Uploads::from_root(dir.path());
        uploads
            .append("upload-1", &BASE64.encode(b"local"), Some(0))
            .unwrap();

        let path = uploads.commit("upload-1", "image.png").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"local");
        assert!(!dir.path().join("tmp").join("upload-1").exists());
    }
}
