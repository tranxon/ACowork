//! Workspace query use case.
//!
//! ADR-040: read-only workspace operations live behind a trait so the
//! HTTP handlers (server.rs) don't touch the filesystem directly. The
//! implementation [`crate::usecases::RuntimeWorkspaceQueryService`] holds
//! the `work_dir` (read once during boot) and resolves workspace IDs to
//! their absolute paths via `agent_workspaces.json`.
//!
//! The types exposed here are the canonical wire-format DTOs for the
//! workspace query endpoints. Field names mirror the public contract
//! used by the desktop `workspaceStore` / `fileEditorStore` (camelCase
//! via `#[serde(rename_all = "camelCase")]` where appropriate). Keep them
//! aligned with the Gateway's `acowork-gateway::http::workspaces` types
//! since the runtime HTTP server is reached via the Gateway reverse
//! proxy — any drift in field names will silently 502 / deserialise to
//! `null` in the desktop.
//!
//! Errors are returned as [`WorkspaceError`] so the HTTP layer can map
//! them to the correct status code without leaking `std::io::Error`'s
//! untyped `Other` into 500s. `WorkspaceError::NotFound` → 404,
//! `WorkspaceError::AlreadyExists` → 409, etc. — see the `to_status_code`
//! helper on the variant.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Re-exports ─────────────────────────────────────────────────────────────
//
// `FilePathQuery` (used by `read_file`, `write_file`, `delete_file`) lives
// in `workspace_mutation` because the write/delete operations live there
// first; we re-export it here so HTTP handlers can use a single import
// (`crate::usecases::workspace_query::FilePathQuery`) for both query and
// mutation routes that share the same querystring shape.
pub use crate::usecases::workspace_mutation::FilePathQuery;

/// All error variants workspace operations can produce.
///
/// The HTTP layer maps each variant to a deterministic status code so
/// the desktop can distinguish "not found" from "internal error" without
/// parsing human-readable error strings.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The requested workspace_id does not exist in `agent_workspaces.json`.
    /// Maps to HTTP 404 (the workspace is unknown) — distinct from
    /// `NotFound` which targets a file/dir path.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// The resolved absolute path does not exist on disk
    /// (file/dir was deleted, or `path` is wrong).
    /// Maps to HTTP 404.
    #[error("not found: {0}")]
    NotFound(String),

    /// A filesystem path component tried to escape the workspace root
    /// (".." traversal, absolute path, etc.).
    /// Maps to HTTP 400.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// The file is not valid UTF-8 — the editor only handles text.
    /// Maps to HTTP 422.
    #[error("invalid UTF-8 in file: {0}")]
    InvalidUtf8(String),

    /// A custom error already includes an HTTP status + message (e.g.
    /// search regex parsing). Maps to the embedded status code.
    #[error("{message}")]
    BadRequest { status: u16, message: String },

    /// Generic I/O failure (read_dir, canonicalize, etc.). Maps to 500.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialisation/deserialisation failure. Maps to 500.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Atomic config write failed. Maps to 500.
    #[error("config persist error: {0}")]
    Persist(String),
}

impl WorkspaceError {
    /// Map this error to an HTTP status code.
    ///
    /// The HTTP layer uses this to choose between 400 / 404 / 409 / 500.
    /// The body of the error becomes the JSON payload's `error` field.
    pub fn http_status(&self) -> u16 {
        match self {
            WorkspaceError::WorkspaceNotFound(_) => 404,
            WorkspaceError::NotFound(_) => 404,
            WorkspaceError::InvalidPath(_) => 400,
            WorkspaceError::InvalidUtf8(_) => 422,
            WorkspaceError::BadRequest { status, .. } => *status,
            WorkspaceError::Io(_) | WorkspaceError::Json(_) | WorkspaceError::Persist(_) => 500,
        }
    }
}

// ── Response DTOs (canonical wire format) ──────────────────────────────────

/// One entry in a workspace directory listing.
#[derive(Debug, Clone, Serialize)]
pub struct TreeEntryDto {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<usize>,
}

/// Response for `list_tree`.
#[derive(Debug, Clone, Serialize)]
pub struct TreeResponse {
    /// Normalised workspace root (no Windows `\\?\` prefix,
    /// forward-slash separators). Exposed verbatim to clients; the
    /// contract mirrors [`FindResponse::root`] and the
    /// `canonical_to_root_string` helper that produces both fields.
    pub root: String,
    pub path: String,
    pub entries: Vec<TreeEntryDto>,
}

/// Querystring for `list_tree`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListTreeParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Response for `list_workspaces`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspacesListResponse {
    /// Echo of the agent_id that owns this listing — useful for the
    /// proxy to log which agent served the request.
    pub agent_id: String,
    /// Raw `additional_dirs` entries from `agent_workspaces.json`,
    /// preserved verbatim so newly-added fields survive round trips.
    pub workspaces: Vec<serde_json::Value>,
}

/// Querystring + body combined for `read_file` (path comes from query).
#[derive(Debug, Clone, Deserialize)]
pub struct ReadFileParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub path: String,
}

// `ReadFileParams` and `FilePathQuery` have identical shapes — the read
// path takes its args from the querystring, the write path takes them
// from a combination of querystring + JSON body. The HTTP handler for
// `GET /workspaces/file` extracts a `FilePathQuery` and converts via
// this `From` impl to call the trait.
impl From<&FilePathQuery> for ReadFileParams {
    fn from(q: &FilePathQuery) -> Self {
        Self {
            workspace_id: q.workspace_id.clone(),
            path: q.path.clone(),
        }
    }
}

/// Response for `read_file` — JSON envelope the desktop expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileDto {
    pub content: String,
    pub size: u64,
    pub mime_type: String,
    pub is_file: bool,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    pub path: String,
}

/// Querystring for `find_files` (filename fuzzy search).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FindFilesParams {
    /// Pattern to match against file/dir name + relative path.
    /// Required — handler validates non-empty before calling the trait.
    #[serde(default)]
    pub q: Option<String>,
    /// Workspace ID. "__agent_home__" or empty = agent home.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Optional cap on returned matches.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Single match in a `find_files` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindMatchDto {
    pub name: String,
    /// Forward-slash relative path within the workspace.
    pub rel_path: String,
    /// `"file"` or `"directory"`.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Heuristic score (higher = better match); client uses for sort.
    pub score: u32,
}

/// Response for `find_files`.
#[derive(Debug, Clone, Serialize)]
pub struct FindResponse {
    /// Normalised workspace root (no Windows `\\?\` prefix).
    pub root: String,
    /// Number of filesystem entries scanned.
    pub scanned: usize,
    /// True when the walk was truncated by the scan cap.
    pub truncated: bool,
    pub matches: Vec<FindMatchDto>,
}

/// Querystring for `search_files` (content / regex search).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchFilesParams {
    /// Regex pattern to search for.
    #[serde(default)]
    pub q: Option<String>,
    /// Workspace ID.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Optional comma-separated file glob filter (e.g. "*.rs,*.toml").
    #[serde(default)]
    pub include: Option<String>,
    /// Maximum number of match results to return (default 200, max 1000).
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Enable case-sensitive matching (default = case-insensitive).
    #[serde(default)]
    pub case_sensitive: bool,
    /// Match whole words only — wraps pattern in `\b…\b`.
    #[serde(default)]
    pub whole_word: bool,
}

/// Single match in a `search_files` response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatchDto {
    /// Relative file path within the workspace.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (byte offset of match start).
    pub column: usize,
    pub text: String,
}

/// Response for `search_files`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub matches: Vec<SearchMatchDto>,
    pub total_matches: usize,
    pub truncated: bool,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// Read-only workspace operations.
///
/// `WorkspaceMutationService` owns the corresponding write operations
/// (create/delete workspaces, files, dirs). Keeping the two split lets
/// the HTTP layer inject a `&dyn WorkspaceQueryService` for read-only
/// endpoints without granting file-mutation access.
#[async_trait]
pub trait WorkspaceQueryService: Send + Sync {
    /// `GET /workspaces` — list workspace directories from
    /// `agent_workspaces.json`.
    async fn list_workspaces(&self) -> Result<WorkspacesListResponse, WorkspaceError>;

    /// `GET /workspaces/tree` — list directory contents.
    async fn list_tree(
        &self,
        params: &ListTreeParams,
    ) -> Result<TreeResponse, WorkspaceError>;

    /// `GET /workspaces/file` — read a UTF-8 text file's content.
    async fn read_file(
        &self,
        params: &ReadFileParams,
    ) -> Result<WorkspaceFileDto, WorkspaceError>;

    /// `GET /workspaces/find` — fuzzy-search file/dir names.
    ///
    /// Implementation walks the workspace with `ignore::WalkBuilder`
    /// (gitignore-aware), scores each entry against the query, and
    /// returns the top `limit` matches sorted by score.
    async fn find_files(
        &self,
        params: &FindFilesParams,
    ) -> Result<FindResponse, WorkspaceError>;

    /// `GET /workspaces/search` — ripgrep-style content search.
    ///
    /// Implementation walks the workspace, reads each text file, and
    /// collects regex matches. Binary files + files larger than 1 MiB
    /// are skipped. Offloaded to `spawn_blocking` for the heavy I/O.
    async fn search_files(
        &self,
        params: &SearchFilesParams,
    ) -> Result<SearchResponse, WorkspaceError>;
}
