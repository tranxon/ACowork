//! Git query use case (ADR-078).
//!
//! Read-only git operations for the Desktop Git Status Bar: `status`,
//! `diff`, `log`. All git execution happens in the **Runtime** (the
//! authoritative workspace owner, ADR-009 v2) via the system git CLI —
//! the Gateway only reverse-proxies these endpoints (ADR-033 Phase 2)
//! and never touches the filesystem.
//!
//! Security invariants (ADR-078 §1.3):
//! - **read-only**: every command runs with `GIT_OPTIONAL_LOCKS=0` so
//!   git cannot take optional locks (e.g. the index stat-cache refresh
//!   `git status` would otherwise perform);
//! - **paths never escape the workspace**: diff/log paths are
//!   canonicalized and must stay under the workspace root; status
//!   output is filtered by the workspace-root prefix before it leaves
//!   the Runtime;
//! - **no absolute paths leak**: response paths are always relative to
//!   the workspace root; `repo_root` is never returned.
//!
//! The types here are the canonical wire-format DTOs for `/git/*`. Field
//! names mirror the contract consumed by the Desktop `gitStore`
//! (camelCase via `#[serde(rename_all = "camelCase")]`). Keep them
//! aligned end-to-end (Runtime HTTP → Gateway reverse proxy → Desktop).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// All error variants git operations can produce.
///
/// The HTTP layer maps each variant to a deterministic status code via
/// [`GitError::http_status`].
#[derive(Debug, Error)]
pub enum GitError {
    /// The requested workspace_id does not exist in `agent_workspaces.json`.
    /// Maps to HTTP 404.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// A filesystem path component tried to escape the workspace root
    /// (".." traversal, absolute path, etc.). Maps to HTTP 400.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// The workspace is not inside a git repository (repo discovery
    /// failed, max 6 levels up). For `/git/status` this surfaces as
    /// `is_repo: false` + `error: "not_a_repo"` inside a 200 response;
    /// for `/git/diff` + `/git/log` it is a real error (400).
    #[error("not a git repository: {0}")]
    NotARepo(String),

    /// The `git` binary is missing / could not be spawned. For
    /// `/git/status` this surfaces as `is_repo: false` +
    /// `error: "git_unavailable"` inside a 200 response; for diff/log
    /// it is a 503.
    #[error("git unavailable: {0}")]
    GitUnavailable(String),

    /// The git command exited non-zero (other than "not a repo").
    #[error("git command failed: {0}")]
    GitFailed(String),

    /// The git command exceeded the timeout budget (default 10s).
    #[error("git command timed out after {0}s")]
    Timeout(u64),

    /// Invalid querystring / parameter combination. Maps to HTTP 400.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Generic I/O failure. Maps to HTTP 500.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Response not valid UTF-8. Maps to HTTP 500.
    #[error("invalid UTF-8: {0}")]
    InvalidUtf8(String),
}

impl GitError {
    /// Map this error to an HTTP status code.
    pub fn http_status(&self) -> u16 {
        match self {
            GitError::WorkspaceNotFound(_) => 404,
            GitError::InvalidPath(_) => 400,
            GitError::NotARepo(_) => 400,
            GitError::GitUnavailable(_) => 503,
            GitError::Timeout(_) => 504,
            GitError::GitFailed(_) | GitError::Io(_) | GitError::InvalidUtf8(_) => 500,
            GitError::BadRequest(_) => 400,
        }
    }
}

// ── Status DTOs ────────────────────────────────────────────────────────────

/// Index (staged) state of a change, mapped from porcelain XY column X.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitIndexStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Unmodified,
}

/// Worktree state of a change, mapped from porcelain XY column Y.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitWorktreeStatus {
    Modified,
    Deleted,
    Untracked,
    Unmodified,
}

/// One changed file in `git status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChangeDto {
    /// Workspace-root-relative path (never absolute, forward slashes).
    pub path: String,
    /// Old path for renames (workspace-root-relative), else None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub index: GitIndexStatus,
    pub worktree: GitWorktreeStatus,
    /// True when the index differs from HEAD (staged).
    pub staged: bool,
}

/// Response for `GET /git/status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResponse {
    pub is_repo: bool,
    /// Current branch name (`HEAD` when detached). None when not a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `"not_a_repo"` | `"git_unavailable"` | null — explicit error
    /// states, never a silent degradation (ADR-078 §1.3 invariant 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True when the porcelain output hit the entry/byte cap and was
    /// truncated (ADR-078 decision 4 — `--untracked-files=all` can
    /// explode on un-ignored directories).
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub changes: Vec<GitChangeDto>,
}

/// Querystring for `GET /git/status`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GitStatusParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
}

// ── Diff DTOs ──────────────────────────────────────────────────────────────

/// How the diff should be rendered by the Monaco DiffEditor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GitDiffKind {
    Modified,
    Untracked,
    Deleted,
    Binary,
    NoChange,
}

/// Response for `GET /git/diff` — two full texts for the DiffEditor
/// (original = HEAD, modified = worktree or index).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResponse {
    pub kind: GitDiffKind,
    /// HEAD version; `""` for untracked; full text for deleted.
    pub original: String,
    /// Worktree version (or index when cached=1); `""` for deleted.
    pub modified: String,
}

/// Querystring for `GET /git/diff`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GitDiffParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Workspace-root-relative path (validated, then converted to
    /// repo-root-relative before reaching git — ADR-078 decision 3).
    pub path: String,
    /// 0 (default): worktree vs HEAD; 1: index vs HEAD (staged diff).
    #[serde(default)]
    pub cached: u8,
}

// ── Log DTOs ───────────────────────────────────────────────────────────────

/// One commit in the file history.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitDto {
    /// Full commit hash.
    pub hash: String,
    /// Abbreviated commit hash (git `%h`).
    pub short_hash: String,
    pub author: String,
    /// ISO 8601 strict commit date (git `%aI`).
    pub date: String,
    pub subject: String,
}

/// Response for `GET /git/log`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitLogResponse {
    pub commits: Vec<GitCommitDto>,
}

/// Querystring for `GET /git/log`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GitLogParams {
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Workspace-root-relative path; empty = repository-wide history.
    #[serde(default)]
    pub path: Option<String>,
    /// Max commits. Default 50, hard cap 200 (ADR-078 decision 4).
    #[serde(default)]
    pub limit: Option<u32>,
}

// ── Service trait ──────────────────────────────────────────────────────────

/// Read-only git query operations, implemented by
/// [`crate::usecases::RuntimeGitQueryService`]. ADR-040 layering: the
/// HTTP handlers depend on this trait, never on the concrete service.
#[async_trait]
pub trait GitQueryService: Send + Sync {
    async fn status(&self, params: &GitStatusParams) -> Result<GitStatusResponse, GitError>;
    async fn diff(&self, params: &GitDiffParams) -> Result<GitDiffResponse, GitError>;
    async fn log(&self, params: &GitLogParams) -> Result<GitLogResponse, GitError>;
}
