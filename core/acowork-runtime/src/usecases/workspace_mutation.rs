//! Workspace mutation use case.
//!
//! ADR-040: write-side workspace operations live behind a trait so the
//! HTTP handlers (server.rs) don't directly mutate `agent_workspaces.json`
//! or touch the filesystem. The implementation
//! [`crate::usecases::RuntimeWorkspaceMutationService`] owns the only
//! `write-rename` path for the config and the only file/dir mutation
//! calls, keeping a single audit point for path-traversal guards and
//! atomic persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::usecases::workspace_query::WorkspaceError;

// ── Request DTOs ───────────────────────────────────────────────────────────

/// One workspace entry as submitted by the desktop.
///
/// `access` is `"read-only"` / `"read-write"` (matches the gateway's
/// `AccessLevel` kebab-case serialisation); unknown variants surface as
/// `WorkspaceError::InvalidPath`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceEntryInput {
    pub id: String,
    pub path: String,
    pub access: String,
    #[serde(default)]
    pub prompt_file: Option<String>,
    #[serde(default)]
    pub last_active: Option<bool>,
}

/// Response for `create_workspace` / `update_workspace`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceMutationResponse {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Echo of the resulting entry as it was persisted (for `create`)
    /// or its prior value (for `update`). Kept as raw JSON so
    /// newly-added fields survive round trips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<serde_json::Value>,
}

/// Request body for `set_prompt_file`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptFileBody {
    #[serde(default)]
    pub prompt_file: Option<String>,
}

/// Body for `create_file`. Path + workspace_id can live in the
/// querystring OR the body (querystring wins if both are set) — this
/// matches the desktop's `buildFileUrl` helper and is documented at
/// the HTTP layer too.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreateFileBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// Body for `write_file` (only `content` is in the body — path +
/// workspace_id are in the querystring).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WriteFileBody {
    #[serde(default)]
    pub content: String,
}

/// Body for `delete_file` + `create_dir` + `delete_dir`.
/// `path` and `workspace_id` may be in querystring or body (querystring
/// wins).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PathOnlyBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Querystring for `read_file` / `write_file` / `delete_file` (path is
/// here so DELETE bodies aren't required for HTTP clients that can't
/// carry one).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FilePathQuery {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: String,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// Write-side workspace operations.
///
/// Atomicity: every mutation persists the workspace config via the
/// `write-tmp-rename` pattern, so concurrent updates from the desktop
/// + Gateway CLI can't lose data. File / dir mutations are bare
/// `std::fs` calls — the path-traversal guard lives in the
/// implementation (see [`crate::usecases::RuntimeWorkspaceMutationService`]).
#[async_trait]
pub trait WorkspaceMutationService: Send + Sync {
    /// `POST /workspaces` — add a new workspace entry.
    /// Returns `AlreadyExists` if `id` collides.
    async fn create_workspace(
        &self,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
    /// Returns `WorkspaceNotFound` if `ws_id` is unknown.
    async fn update_workspace(
        &self,
        ws_id: &str,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/{ws_id}/prompt-file` — set the prompt_file
    /// field on a workspace entry (or clear it via `null`).
    async fn set_prompt_file(
        &self,
        ws_id: &str,
        body: PromptFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/{ws_id}` — remove a workspace entry.
    async fn delete_workspace(&self, ws_id: &str)
        -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/file` — create a new text file.
    /// Returns `AlreadyExists` if the file exists and `overwrite=false`.
    async fn create_file(
        &self,
        body: CreateFileBody,
        query_ws: Option<&str>,
        query_path: Option<&str>,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/file` — overwrite an existing text file.
    /// Returns `NotFound` if the file is missing.
    async fn write_file(
        &self,
        query: FilePathQuery,
        body: WriteFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/file` — remove a file.
    async fn delete_file(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/dir` — create a directory (recursive).
    async fn create_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/dir` — remove a directory recursively.
    async fn delete_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;
}