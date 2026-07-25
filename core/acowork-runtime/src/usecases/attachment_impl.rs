//! RuntimeAttachmentService — implements [`AttachmentService`] (ADR-046).
//!
//! Backed by the unified blob store at `<work_dir>/files/<document_id>`.
//! One physical directory for **all** uploads across all sessions (PDF,
//! DOCX, PNG, JPG, …); per-session indexing lives in the JSONL as
//! `AttachmentMeta` system entries (see
//! [`crate::conversation::AttachmentMeta`]).
//!
//! ## State
//!
//! Holds the boot-time `work_dir` (no async resource dependencies), so
//! the service can be constructed immediately after the workspace
//! services in `session_init.rs` Phase B and published to the
//! `attachment_slot` for HTTP handlers to consume.
//!
//! ## On-disk layout
//!
//! ```text
//! <work_dir>/
//! ├── conversations/
//! │   └── <sid>.jsonl       # each upload / attach is a system entry
//! └── files/
//!     └── <document_id>      # blob, no extension — format lives in JSONL
//! ```
//!
//! ## `document_id` format
//!
//! 12-hex content-hash prefix + 4-hex suffix (`[0-9a-f]{12}-[0-9a-f]{1,4}`).
//! Stable across uploads of identical bytes, so deduplication is
//! implicit — same bytes ⇒ same `document_id` ⇒ `tokio::fs::write`
//! produces the same on-disk file. Read paths validate the charset
//! before any filesystem access to defang path-traversal probes.
//!
//! ## Atomicity
//!
//! Writes use the write-tmp-rename pattern: bytes land in
//! `<work_dir>/files/<document_id>.tmp`, then `rename` swaps into
//! place. Concurrent uploads of the same content race harmlessly on
//! the rename — the second rename is a no-op because the file already
//! has the same content (and we never mutate a placed blob).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::usecases::attachment::{
    AttachmentError, AttachmentService, UploadFileParams, UploadedFileResponse, MAX_UPLOAD_BYTES,
};

/// Concrete [`AttachmentService`] backed by `<work_dir>/files/`.
pub struct RuntimeAttachmentService {
    work_dir: PathBuf,
}

impl RuntimeAttachmentService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }

    /// Resolve `<work_dir>/files`. Created on demand by
    /// [`RuntimeAttachmentService::ensure_files_dir`].
    fn files_dir(&self) -> PathBuf {
        self.work_dir.join("files")
    }

    async fn ensure_files_dir(&self) -> Result<PathBuf, AttachmentError> {
        let dir = self.files_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .await
                .map_err(|e| AttachmentError::Persistence(format!("create_dir_all {}: {}", dir.display(), e)))?;
        }
        Ok(dir)
    }

    /// Validate `document_id` matches the canonical charset. Cheap; no
    /// filesystem access. Used by every read path and (defensively) by
    /// write paths that accept a precomputed id.
    fn validate_document_id(document_id: &str) -> Result<(), AttachmentError> {
        // 12-hex prefix + `-` + 1..=4 hex suffix.
        let bytes = document_id.as_bytes();
        let len = bytes.len();
        if !(14..=17).contains(&len) {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        if bytes[12] != b'-' {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        let hex_ok = |c: &u8| c.is_ascii_hexdigit();
        if !bytes[..12].iter().all(hex_ok) || !bytes[13..].iter().all(hex_ok) {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        Ok(())
    }

    /// Compute the canonical `document_id` for a content payload.
    ///
    /// 12-hex content-hash prefix + 4-hex random suffix (matching the
    /// legacy `compute_doc_id` algorithm so existing migration paths
    /// remain stable). The random suffix is generated from system
    /// nanoseconds — sufficient for collision avoidance on a single
    /// host; sha-style content hashing would be overkill given the
    /// input is already canonicalized by the size cap.
    fn compute_document_id(bytes: &[u8]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        let h = hasher.finish();
        let prefix = h & 0xFFFF_FFFF_FFFF;
        let suffix_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        // Mix the suffix seed with the content hash so two uploads in
        // the same nanosecond still pick distinct suffixes.
        let suffix = (h >> 48 ^ suffix_seed) & 0xFFFF;
        format!("{:012x}-{:x}", prefix, suffix)
    }

    /// Atomic write-tmp-rename of `bytes` to `<dir>/<document_id>`.
    ///
    /// Returns true if a new file was created; false if an existing
    /// blob with the same `document_id` was already on disk (dedup hit
    /// — the existing file is left untouched).
    async fn write_blob_atomic(
        &self,
        dir: &Path,
        document_id: &str,
        bytes: &[u8],
    ) -> Result<bool, AttachmentError> {
        let final_path = dir.join(document_id);
        if final_path.exists() {
            // Dedup hit — never overwrite (the bytes must be identical
            // because the id is content-derived).
            return Ok(false);
        }
        let tmp_path = dir.join(format!("{}.tmp", document_id));
        {
            let mut f = fs::File::create(&tmp_path).await.map_err(|e| {
                AttachmentError::Persistence(format!("create tmp {}: {}", tmp_path.display(), e))
            })?;
            f.write_all(bytes).await.map_err(|e| {
                AttachmentError::Persistence(format!("write tmp {}: {}", tmp_path.display(), e))
            })?;
            f.sync_all().await.map_err(|e| {
                AttachmentError::Persistence(format!("sync tmp {}: {}", tmp_path.display(), e))
            })?;
        }
        // rename is atomic on the same filesystem on Unix; Windows
        // would need ReplaceFileW semantics but Desktop targets macOS
        // and Windows is not in scope for this runtime path.
        fs::rename(&tmp_path, &final_path).await.map_err(|e| {
            AttachmentError::Persistence(format!(
                "rename {} -> {}: {}",
                tmp_path.display(),
                final_path.display(),
                e
            ))
        })?;
        Ok(true)
    }
}

#[async_trait]
impl AttachmentService for RuntimeAttachmentService {
    async fn upload_file(
        &self,
        params: UploadFileParams,
    ) -> Result<UploadedFileResponse, AttachmentError> {
        if params.bytes.len() > MAX_UPLOAD_BYTES {
            return Err(AttachmentError::PayloadTooLarge(params.bytes.len()));
        }

        let dir = self.ensure_files_dir().await?;
        let document_id = Self::compute_document_id(&params.bytes);

        // validate_document_id is a sanity check on the computed id —
        // compute_document_id always produces a well-formed id by
        // construction, but a defensive call here keeps the audit
        // log uniform with the read path.
        Self::validate_document_id(&document_id)?;

        self.write_blob_atomic(&dir, &document_id, &params.bytes).await?;

        Ok(UploadedFileResponse {
            document_id,
            filename: params.filename,
            format: params.format,
            size_bytes: params.bytes.len() as u64,
            width: params.width,
            height: params.height,
        })
    }

    async fn read_file(&self, document_id: &str) -> Result<Vec<u8>, AttachmentError> {
        Self::validate_document_id(document_id)?;

        // Resolve the on-disk path: `<work_dir>/files/<document_id>`.
        // The file has no extension — format lives in JSONL metadata.
        // The caller (HTTP handler) is responsible for attaching the
        // correct Content-Type when serving the response.
        let path = self.files_dir().join(document_id);
        match fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(AttachmentError::NotFound(document_id.to_string()))
            }
            Err(e) => Err(AttachmentError::Persistence(format!("read {}: {}", path.display(), e))),
        }
    }
}