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
//!     └── <document_id>.<ext>  # blob — ext is a sanitised whitelisted
//!                              # suffix chosen from the user's `format`
//!                              # (see [`RuntimeAttachmentService::safe_extension`])
//! ```
//!
//! Pre-046 files written as bare `<document_id>` (no extension) still
//! exist on some developer disks. `read_file` falls back to that name
//! so historic blobs remain readable; new writes always pick the
//! suffixed name and reuse the legacy file as a dedup hit (its bytes
//! are identical because `document_id` is content-derived).
//!
//! ## `document_id` format
//!
//! 12-hex content-hash prefix + 4-hex content-hash suffix
//! (`[0-9a-f]{12}-[0-9a-f]{4}`). Both halves are sliced from a single
//! `SHA-256(bytes)` digest, so the id is **fully content-derived**:
//! identical bytes always yield the same `document_id`, regardless of
//! process, host, or wall-clock time. This is the cornerstone of the
//! implicit-dedup invariant — same bytes ⇒ same id ⇒ `tokio::fs::rename`
//! onto an existing `<document_id>.<ext>` is a no-op.
//!
//! Why SHA-256 (not `std::collections::hash_map::DefaultHasher`):
//! `DefaultHasher` is `SipHash` with `RandomState`-derived keys that
//! are reseeded per process. Across a Runtime restart the same bytes
//! would hash to a **different** prefix, defeating dedup across
//! sessions. SHA-256 is collision-resistant and deterministic — the
//! only choice for a stable content address.
//!
//! Why 48+16 bits (and not, say, 256 bits): the storage layer can only
//! sanity-check the prefix against on-disk filenames; longer ids would
//! just inflate every JSONL metadata entry and HTTP URL without
//! improving practical collision odds (a user-side file library of 2^24
//! ≈ 16M blobs still has ~50% odds of a 48-bit collision, but no single
//! agent reaches that volume). If a collision ever does happen, the
//! read path's "ambiguous on-disk match" guard surfaces it as an
//! error rather than silently picking one.
//!
//! Read paths validate the charset before any filesystem access to
//! defang path-traversal probes.
//!
//! ## Atomicity
//!
//! Writes use the write-tmp-rename pattern: bytes land in
//! `<work_dir>/files/<document_id>.<ext>.tmp`, then `rename` swaps into
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
    /// `document_id = format!("{prefix_hex}-{suffix_hex}")` where both
    /// halves are taken from `SHA-256(bytes)`:
    ///
    /// - `prefix_hex` = first 6 bytes → 12 lowercase hex chars (48 bits).
    /// - `suffix_hex` = next 2 bytes → 4 lowercase hex chars (16 bits).
    ///
    /// Total id length: 12 + 1 (dash) + 4 = 17 chars, which satisfies the
    /// 14..=17 length window accepted by [`Self::validate_document_id`].
    ///
    /// Stability: identical bytes ⇒ identical id, regardless of process,
    /// wall-clock time, or system time-jitter. Verified by
    /// `compute_document_id_is_content_deterministic`.
    fn compute_document_id(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let prefix_hex: String = digest[..6]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let suffix_hex: String = digest[6..8]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        format!("{prefix_hex}-{suffix_hex}")
    }

    /// Atomic write-tmp-rename of `bytes` to `<dir>/<document_id>.<ext>`.
    ///
    /// `format` selects the on-disk suffix via
    /// [`RuntimeAttachmentService::safe_extension`].  Returns true if a
    /// new file was created; false if a blob with the same
    /// `document_id` was already on disk (dedup hit — the existing
    /// file is left untouched).  A pre-046 bare `<document_id>` file is
    /// also treated as a dedup hit for backward compatibility.
    async fn write_blob_atomic(
        &self,
        dir: &Path,
        document_id: &str,
        format: &str,
        bytes: &[u8],
    ) -> Result<bool, AttachmentError> {
        let ext = Self::safe_extension(format);
        let final_path = dir.join(format!("{document_id}.{ext}"));
        if final_path.exists() {
            // Dedup hit — never overwrite (the bytes must be identical
            // because the id is content-derived).
            return Ok(false);
        }
        // Legacy compatibility: if a pre-046 bare file (no extension)
        // with the same document_id exists, treat the upload as a
        // dedup hit. This keeps dedup semantics stable for users who
        // uploaded files before this version landed — the underlying
        // bytes are identical by construction.
        let legacy_path = dir.join(document_id);
        if legacy_path.exists() {
            return Ok(false);
        }
        let tmp_path = dir.join(format!("{document_id}.{ext}.tmp"));
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

    /// Map a user-supplied `format` (lowercased, no leading dot — e.g.
    /// `"jpg"`, `"pdf"`) to a safe on-disk extension suffix.
    ///
    /// The mapping is whitelisted rather than pass-through so a hostile
    /// or accidental format string (e.g. `"exe"`, `"html"`, `"./.."`)
    /// never lands as a Finder / shell-trusted extension. Anything not
    /// on the whitelist collapses to `"bin"` — the file is still
    /// readable, just not double-click-previewable.
    ///
    /// This list covers every format the desktop dialog filter offers
    /// (`pdf docx pptx xlsx png jpg jpeg gif webp`). New formats
    /// should be added here intentionally, not by passing the raw
    /// user value through.
    fn safe_extension(format: &str) -> &'static str {
        match format.to_ascii_lowercase().as_str() {
            // Documents — must stay in lock-step with `doc_reader`'s
            // `detect_format` whitelist (`tools/builtin/doc_reader/mod.rs`).
            // The runtime-side `doc_reader` tool reads these blobs by
            // absolute path and picks the extractor from the file
            // extension. If a blob lands as `.bin` here, doc_reader will
            // reject it as "Unsupported document format" and the LLM
            // gets a clean error instead of the user's PDF/DOCX content.
            "pdf" => "pdf",
            "docx" => "docx",
            "pptx" => "pptx",
            "xlsx" => "xlsx",
            // Images — same rationale: `derive_image_parts` reads the
            // blob and wraps it in a `data:` URI whose MIME tag is the
            // document `format` (not the on-disk suffix), so the file
            // extension is informational here, but keeping them explicit
            // makes Finder preview behave correctly.
            "png" => "png",
            "jpg" | "jpeg" => "jpg",
            "gif" => "gif",
            "webp" => "webp",
            // Everything else collapses to `bin` — see the doc comment
            // on the surrounding match arm for the threat model.
            _ => "bin",
        }
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

        self.write_blob_atomic(&dir, &document_id, &params.format, &params.bytes)
            .await?;

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

        // Resolve the on-disk path. New writes land at
        // `<work_dir>/files/<document_id>.<safe_ext>`; pre-046 uploads
        // may still exist as a bare `<document_id>` file. `validate_document_id`
        // already guards against path traversal, and the entry-name
        // prefix check below is defensive: only filenames that begin
        // with `<document_id>` (with or without a single `.`-followed-
        // by-anything suffix) are accepted.
        let dir = self.files_dir();
        let prefix = document_id.to_string();
        let mut matched: Option<PathBuf> = None;
        let mut entries = fs::read_dir(&dir).await.map_err(|e| {
            AttachmentError::Persistence(format!("read_dir {}: {}", dir.display(), e))
        })?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            AttachmentError::Persistence(format!("iter dir {}: {}", dir.display(), e))
        })? {
            // Bind `file_name` first — `OsString::to_str` borrows the
            // `OsString` which is dropped at end of statement otherwise.
            let file_name_os = entry.file_name();
            let Some(name) = file_name_os.to_str() else { continue };
            let is_match = name == prefix
                || (name.len() > prefix.len()
                    && name.starts_with(&prefix)
                    && name.as_bytes()[prefix.len()] == b'.');
            if !is_match {
                continue;
            }
            match matched {
                None => matched = Some(entry.path()),
                Some(_) => {
                    return Err(AttachmentError::Persistence(format!(
                        "ambiguous on-disk match for document_id {document_id:?} in {}",
                        dir.display()
                    )));
                }
            }
        }
        let Some(path) = matched else {
            return Err(AttachmentError::NotFound(document_id.to_string()));
        };
        match fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(AttachmentError::NotFound(document_id.to_string()))
            }
            Err(e) => Err(AttachmentError::Persistence(format!("read {}: {}", path.display(), e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `AttachmentService` rooted in `tempfile::TempDir` so the
    /// test owns the directory and cleans up on drop.
    async fn fresh_service() -> (tempfile::TempDir, RuntimeAttachmentService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = RuntimeAttachmentService::new(dir.path().to_path_buf());
        (dir, svc)
    }

    fn params(format: &str, payload: &[u8]) -> UploadFileParams {
        UploadFileParams {
            filename: format!("sample.{format}"),
            format: format.to_string(),
            bytes: payload.to_vec(),
            width: None,
            height: None,
        }
    }

    /// Whitelisted formats get a real extension; everything else gets `bin`.
    /// This guards against a hostile / accidental `"exe"` / `"html"` slipping
    /// through into a Finder-trusted extension.
    ///
    /// The doc formats (`pdf docx pptx xlsx`) MUST appear here so the
    /// `doc_reader` tool can dispatch by file extension — see
    /// `tools/builtin/doc_reader/mod.rs::detect_format`. A `docx` blob
    /// landing as `.bin` would make doc_reader report
    /// `"Unsupported document format"` even though the bytes are valid.
    #[test]
    fn safe_extension_is_whitelisted() {
        for (input, expected) in [
            // Images
            ("jpg", "jpg"),
            ("JPG", "jpg"),
            ("jpeg", "jpg"),
            ("png", "png"),
            ("gif", "gif"),
            ("webp", "webp"),
            // Documents — case-insensitive to be safe against a Tauri /
            // desktop front-end that didn't lowercase.
            ("pdf", "pdf"),
            ("PDF", "pdf"),
            ("docx", "docx"),
            ("DOCX", "docx"),
            ("pptx", "pptx"),
            ("PPTX", "pptx"),
            ("xlsx", "xlsx"),
            ("XLSX", "xlsx"),
            // Whitelist reject:
            ("exe", "bin"),
            ("html", "bin"),
            ("", "bin"),
            ("../../etc/passwd", "bin"),
            ("./jsp", "bin"),
        ] {
            assert_eq!(
                RuntimeAttachmentService::safe_extension(input),
                expected,
                "format={input:?} → expected {expected:?}"
            );
        }
    }

    /// End-to-end coverage that docx/pptx/xlsx blobs land with their
    /// real Office extensions — this is the regression test for the bug
    /// where every document format silently degraded to `.bin`, which
    /// then made the `doc_reader` tool reject them as unsupported.
    #[tokio::test]
    async fn upload_writes_office_docs_with_real_extensions() {
        for (format, expected_ext) in [("pdf", "pdf"), ("docx", "docx"), ("pptx", "pptx"), ("xlsx", "xlsx")] {
            let (dir, svc) = fresh_service().await;
            let payload = format!("hello-{format}-bytes").into_bytes();
            let r = svc.upload_file(params(format, &payload)).await.unwrap();
            let on_disk = dir.path().join("files").join(format!("{}.{expected_ext}", r.document_id));
            assert!(
                on_disk.exists(),
                "upload({format}) must land at {} (got doc_id={})",
                on_disk.display(),
                r.document_id
            );
        }
    }

    /// `upload_file` writes to `<dir>/<document_id>.<ext>` with the
    /// whitelisted suffix. Dedup behaviour (same bytes ⇒ same id) is
    /// covered by [`upload_then_reupload_same_bytes_dedups`] below.
    #[tokio::test]
    async fn upload_writes_with_whitelisted_extension() {
        let (dir, svc) = fresh_service().await;
        let payload = b"hello-image-bytes";
        let r = svc.upload_file(params("jpg", payload)).await.unwrap();
        assert!(r.document_id.contains('-'));
        let on_disk = dir.path().join("files").join(format!("{}.jpg", r.document_id));
        if !on_disk.exists() {
            // Build a helpful diagnostic list of files for the failure message.
            let mut names = Vec::new();
            let mut entries = fs::read_dir(dir.path().join("files")).await.unwrap();
            while let Some(e) = entries.next_entry().await.unwrap() {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
            panic!(
                "expected blob at {}; dir contents: {names:?}",
                on_disk.display(),
            );
        }
    }

    /// Non-whitelisted formats fall back to `.bin` so an accidental
    /// raw string cannot surface as a Finder-rendered extension.
    #[tokio::test]
    async fn upload_unknown_format_falls_back_to_bin_extension() {
        let (dir, svc) = fresh_service().await;
        let r = svc.upload_file(params("exe", b"binary-blob")).await.unwrap();
        let on_disk = dir.path().join("files").join(format!("{}.bin", r.document_id));
        assert!(on_disk.exists(), "expected {} to exist", on_disk.display());
    }

    /// `read_file` resolves both the new suffixed blob AND the
    /// pre-046 bare-name legacy blob to the same bytes; ambiguous
    /// collisions return a persistence error rather than guessing.
    #[tokio::test]
    async fn read_resolves_suffixed_and_legacy_blobs() {
        let (dir, svc) = fresh_service().await;
        let payload = b"read-me";
        let r = svc.upload_file(params("png", payload)).await.unwrap();

        // New suffixed path is what `read_file` finds first.
        let got = svc.read_file(&r.document_id).await.unwrap();
        assert_eq!(got, payload);

        // Now simulate a legacy bare file: delete the suffixed file and
        // write a legacy <doc_id> blob with the same content. read_file
        // must still find it.
        tokio::fs::remove_file(dir.path().join("files").join(format!("{}.png", r.document_id)))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("files").join(&r.document_id), payload)
            .await
            .unwrap();
        let got_legacy = svc.read_file(&r.document_id).await.unwrap();
        assert_eq!(got_legacy, payload);
    }

    /// Regression guard for the dedup invariant: uploading identical
    /// bytes twice (whether back-to-back or "after a restart" — same
    /// process is enough since the hash is content-only) MUST collapse
    /// to a single on-disk file and a single `document_id`. Before the
    /// SHA-256 rewrite this failed because the old implementation XOR'd
    /// `SystemTime::subsec_nanos()` into the suffix.
    #[tokio::test]
    async fn upload_then_reupload_same_bytes_dedups() {
        let (dir, svc) = fresh_service().await;
        let payload = b"identical-image-bytes-for-dedup-check";

        let first = svc.upload_file(params("png", payload)).await.unwrap();
        let second = svc.upload_file(params("png", payload)).await.unwrap();

        assert_eq!(
            first.document_id, second.document_id,
            "identical bytes must hash to the same document_id"
        );

        // And there must be exactly one file on disk matching that id
        // — i.e. the second upload was a true dedup hit, not a
        // co-located duplicate.
        let suffixed = dir.path().join("files").join(format!("{}.png", first.document_id));
        assert!(suffixed.exists(), "suffixed blob must exist");
        let entries = tokio::fs::read_dir(dir.path().join("files"))
            .await
            .unwrap();
        let mut matches = 0usize;
        let mut entries = entries;
        while let Some(e) = entries.next_entry().await.unwrap() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n == first.document_id || n == format!("{}.png", first.document_id) {
                matches += 1;
            }
        }
        assert_eq!(
            matches, 1,
            "exactly one on-disk file must carry this document_id"
        );
    }

    /// Regression guard at the pure-function level: identical bytes
    /// must always hash to the same id, even when called many times in
    /// a row (the old buggy implementation drifted each call because of
    /// the per-call `SystemTime::now()` read). Also asserts that
    /// distinct bytes almost certainly produce distinct ids — we use
    /// a tiny-but-different payload and rely on the 48-bit prefix
    /// making a collision astronomically unlikely.
    #[test]
    fn compute_document_id_is_content_deterministic() {
        let a = b"alpha-bytes";
        let b = b"beta-bytes";

        // Stability across repeated calls.
        let id_a_1 = RuntimeAttachmentService::compute_document_id(a);
        let id_a_2 = RuntimeAttachmentService::compute_document_id(a);
        let id_a_3 = RuntimeAttachmentService::compute_document_id(a);
        assert_eq!(id_a_1, id_a_2);
        assert_eq!(id_a_2, id_a_3);

        // Format: 12 hex + `-` + 4 hex (17 chars total), all lowercase.
        assert_eq!(id_a_1.len(), 17, "expected 17-char id, got {id_a_1:?}");
        let (prefix, suffix) = id_a_1.split_once('-').expect("id contains dash");
        assert_eq!(prefix.len(), 12);
        assert_eq!(suffix.len(), 4);
        assert!(
            prefix.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "prefix must be lowercase hex: {prefix:?}"
        );
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "suffix must be lowercase hex: {suffix:?}"
        );

        // Distinct content → distinct id (collision odds ≈ 2^-48).
        let id_b = RuntimeAttachmentService::compute_document_id(b);
        assert_ne!(id_a_1, id_b, "distinct bytes must hash differently");

        // Validate against the charset guard.
        RuntimeAttachmentService::validate_document_id(&id_a_1).expect("computed id must validate");
        RuntimeAttachmentService::validate_document_id(&id_b).expect("computed id must validate");
    }

    /// Distinct bytes that happen to share the first 6 hash bytes
    /// (i.e. a 48-bit collision) would be indistinguishable by `read_file`
    /// under the existing prefix-based filename scheme. With SHA-256
    /// first-6-byte slicing the only way to hit this is for the 6 bytes
    /// to be byte-identical, which is a 1-in-2^48 event; we don't try
    /// to craft such an input here. This test documents the
    /// expectation with a much weaker sanity check: the helper must
    /// produce valid ids for an empty payload too (boundary case that
    /// previously fell into the `{:x}` zero-padding branch).
    #[test]
    fn compute_document_id_handles_empty_payload() {
        let id = RuntimeAttachmentService::compute_document_id(b"");
        assert_eq!(id.len(), 17);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // The empty-string SHA-256 prefix is e3b0c442… — verify the
        // first 12 chars are exactly that, pinning the implementation
        // against accidental algorithm swaps.
        assert_eq!(
            &id[..12],
            "e3b0c44298fc",
            "empty-payload id must match SHA-256(\"\") prefix"
        );
    }

    /// `read_file` rejects malformed `document_id` values before any
    /// filesystem access (defang path-traversal probes).
    #[tokio::test]
    async fn read_rejects_path_traversal_probe() {
        let (_dir, svc) = fresh_service().await;
        for bad in ["../../../etc/passwd", "abc", ""] {
            let err = svc.read_file(bad).await.unwrap_err();
            assert!(
                matches!(err, AttachmentError::InvalidDocumentId(_)),
                "expected InvalidDocumentId for {bad:?}, got {err:?}"
            );
        }
    }
}