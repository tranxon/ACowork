//! Attachment storage use case (ADR-046).
//!
//! Owns all read/write operations on the unified attachment blob store at
//! `<work_dir>/files/<document_id>`. The companion implementation
//! [`crate::usecases::RuntimeAttachmentService`] is the single audit point
//! for doc-id computation, dedup, atomic persistence, and the
//! `document_id` charset guard (used to defang path-traversal probes).
//!
//! ## Why this is a trait
//!
//! ADR-040: HTTP handlers depend on UseCase traits, never on concrete
//! functions or `std::fs` calls directly. Without this trait, the new
//! `POST /sessions/{sid}/files` and `GET /files/{doc_id}` handlers would
//! re-introduce the "direct-call" antipattern flagged by the
//! MCP/Search-config fix (see [`crate::usecases::AgentToolsService`] for
//! the established pattern).
//!
//! ## What lives here
//!
//! | HTTP endpoint | Trait method | On-disk location |
//! |---|---|---|
//! | `POST /sessions/{sid}/files` (multipart) | [`AttachmentService::upload_file`] | `<work_dir>/files/<document_id>` |
//! | `GET  /files/{document_id}`            | [`AttachmentService::read_file`]   | `<work_dir>/files/<document_id>` |
//!
//! `DELETE /files/{document_id}` is intentionally **out of scope** —
//! ADR-046 §5 defers file lifecycle management (cleanup, dedup
//! compaction, expiry) to a future document-management feature.
//! Adding `delete_file` now would be YAGNI.
//!
//! ## Wiring
//!
//! The implementation only needs the boot-time `work_dir` (sync, no
//! async resource dependency), so it publishes into the late-bind slot
//! alongside `agent_tools` / `agent_config` in `session_init.rs`
//! Phase B. Pre-Phase-B HTTP probes receive `503 Service Unavailable`
//! from the slot-first handler pattern, matching every other
//! `*Service` handler in this crate.

use async_trait::async_trait;

/// Maximum upload size for a single file (50 MiB).
///
/// Matches the legacy `upload_document` limit and accommodates a
/// reasonably-large PDF/PPTX while keeping the runtime honest about
/// what it accepts without consuming gigabytes of disk. The check is
/// performed **after** multipart parsing on the accumulated
/// in-memory byte buffer; rejection returns
/// [`AttachmentError::PayloadTooLarge`].
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// Errors a caller of [`AttachmentService`] can receive.
///
/// Each variant maps deterministically to an HTTP status:
/// - [`AttachmentError::ServiceUnavailable`] → 503
/// - [`AttachmentError::PayloadTooLarge`]    → 413
/// - [`AttachmentError::InvalidDocumentId`]  → 400
/// - [`AttachmentError::Persistence`]        → 500
/// - [`AttachmentError::NotFound`]           → 404
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    /// The late-bind slot has not been populated yet (Phase B has not
    /// run). HTTP 503 — the runtime is not ready.
    #[error("attachment service not ready")]
    ServiceUnavailable,

    /// Upload exceeded [`MAX_UPLOAD_BYTES`]. HTTP 413.
    #[error("upload too large: {0} bytes (limit {MAX_UPLOAD_BYTES})")]
    PayloadTooLarge(usize),

    /// `document_id` is malformed (charset / length violation). HTTP
    /// 400 — protects against path traversal on the read path.
    #[error("invalid document_id: {0:?}")]
    InvalidDocumentId(String),

    /// I/O failure reading or writing the blob store. HTTP 500.
    #[error("attachment persistence error: {0}")]
    Persistence(String),

    /// The blob for `document_id` does not exist on disk. HTTP 404.
    #[error("attachment not found: {0}")]
    NotFound(String),
}

// ── Request / response DTOs ────────────────────────────────────────────────

/// Parameters for [`AttachmentService::upload_file`].
///
/// All fields except `bytes` are required for JSONL persistence: the
/// frontend reads `metadata` to render the chip / thumbnail and the
/// LLM uses the file via tools. `width` / `height` are optional so a
/// non-desktop client can omit them — the renderer falls back to
/// `<img onLoad>` natural sizing (ADR-046 §2.5).
#[derive(Debug, Clone)]
pub struct UploadFileParams {
    /// Filename as supplied by the user (UI hint only; not used as a
    /// filesystem path component).
    pub filename: String,
    /// Lowercase extension without the dot (e.g. "pdf", "png").
    pub format: String,
    /// Raw bytes of the file.
    pub bytes: Vec<u8>,
    /// `width` for image uploads (image files only); `None` for
    /// documents or when the client cannot measure.
    pub width: Option<u32>,
    /// `height` for image uploads; same optionality rules as `width`.
    pub height: Option<u32>,
}

/// Response for [`AttachmentService::upload_file`].
///
/// Mirrors the `AttachedItem::FileUpload` / `ImageUpload` wire format
/// (camelCase via serde) so the desktop can hand it straight to its
/// upload command without re-marshalling.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedFileResponse {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
    /// Echo of the optional image dimensions, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// UseCase trait for attachment blob storage (ADR-046).
///
/// Implementations MUST:
/// - place uploaded files at `<work_dir>/files/<document_id>` (no
///   session subdirectory — JSONL is the per-session index),
/// - validate that `document_id` matches `[0-9a-f]{12}-[0-9a-f]{1,4}`
///   before any filesystem access,
/// - persist atomically (write-tmp-rename),
/// - dedup by content hash (same bytes ⇒ same `document_id` ⇒
///   existing file is returned as-is).
#[async_trait]
pub trait AttachmentService: Send + Sync {
    /// `POST /sessions/{sid}/files` — store a new upload.
    ///
    /// Returns the persisted blob's metadata. The HTTP handler is
    /// expected to forward the response to the desktop verbatim. The
    /// implementation is responsible for:
    /// - size validation ([`MAX_UPLOAD_BYTES`]),
    /// - content-hash doc-id computation,
    /// - atomic write-tmp-rename to `<work_dir>/files/<document_id>`,
    /// - dedup detection (no re-write when `document_id` already
    ///   exists).
    async fn upload_file(&self, params: UploadFileParams) -> Result<UploadedFileResponse, AttachmentError>;

    /// `GET /files/{document_id}` — read a previously-uploaded blob.
    ///
    /// Returns the raw bytes. The on-disk file has no extension (format
    /// lives in JSONL metadata) so this trait cannot tell callers the
    /// MIME type — the HTTP layer / frontend supplies it from the
    /// `AttachmentMeta` they already have on hand when issuing the
    /// read.
    async fn read_file(&self, document_id: &str) -> Result<Vec<u8>, AttachmentError>;
}