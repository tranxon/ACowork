//! RuntimeWorkspaceMutationService — implements [`WorkspaceMutationService`].
//!
//! ADR-040: the only implementation of the workspace mutation trait.
//! Holds the agent `work_dir` and provides a single audit point for:
//! - atomic `agent_workspaces.json` persistence (write-tmp-rename)
//! - file/dir mutation calls that go through the path-traversal guard
//!
//! The query counterpart [`crate::usecases::RuntimeWorkspaceQueryService`]
//! owns the `resolve_within` helper used here; mutations reuse it so
//! the path-traversal guard is identical for reads and writes.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::usecases::workspace_mutation::{
    CreateFileBody, FilePathQuery, PathOnlyBody, PromptFileBody, WorkspaceEntryInput,
    WorkspaceMutationResponse, WorkspaceMutationService,
};
use crate::usecases::workspace_query::WorkspaceError;

pub struct RuntimeWorkspaceMutationService {
    work_dir: PathBuf,
}

impl RuntimeWorkspaceMutationService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }

    /// Resolve path + workspace_id preferring querystring over body.
    /// Returns `(workspace_id, path)` with empty `path` if neither side
    /// supplied one. The caller is responsible for the "missing path"
    /// error.
    fn resolve_path_workspace(
        query_ws: Option<&str>,
        query_path: Option<&str>,
        body_ws: Option<&str>,
        body_path: Option<&str>,
    ) -> (Option<String>, String) {
        let workspace_id = query_ws
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| body_ws.filter(|s| !s.is_empty()).map(|s| s.to_string()));
        let path = query_path
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| body_path.filter(|s| !s.is_empty()).map(|s| s.to_string()))
            .unwrap_or_default();
        (workspace_id, path)
    }

    /// Resolve + path-traversal guard. Reuses the same logic as
    /// [`RuntimeWorkspaceQueryService::resolve_within`] so mutations
    /// and queries share a single security boundary.
    fn resolve_within(
        &self,
        workspace_id: Option<&str>,
        requested_path: &str,
    ) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
        // The query service holds the work_dir too, but its API is
        // centered around HTTP responses. Re-implementing the same
        // path-traversal guard here keeps the two impls decoupled —
        // callers don't need a query service to do mutations.
        resolve_within_static(&self.work_dir, workspace_id, requested_path)
    }

    // ── Config (de)serialisation ──────────────────────────────────────

    fn workspaces_config_path(&self) -> PathBuf {
        self.work_dir.join("config").join("agent_workspaces.json")
    }

    fn load_config(&self) -> WorkspacesConfig {
        let path = self.workspaces_config_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<WorkspacesConfig>(&s).ok())
            .unwrap_or_default()
    }

    fn save_config(&self, cfg: &WorkspacesConfig) -> Result<(), WorkspaceError> {
        let dir = self.work_dir.join("config");
        std::fs::create_dir_all(&dir)
            .map_err(|e| WorkspaceError::Persist(format!("create config dir: {}", e)))?;
        let path = self.workspaces_config_path();
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(cfg)
            .map_err(WorkspaceError::Json)?;
        std::fs::write(&tmp, &json)
            .map_err(|e| WorkspaceError::Persist(format!("write tmp: {}", e)))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| WorkspaceError::Persist(format!("rename tmp: {}", e)))?;
        Ok(())
    }
}

// ── Workspace config on-disk schema ───────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct WorkspacesConfig {
    #[serde(default = "default_workspaces_version")]
    version: u32,
    #[serde(default)]
    additional_dirs: Vec<serde_json::Value>,
}

fn default_workspaces_version() -> u32 {
    1
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            version: 1,
            additional_dirs: Vec::new(),
        }
    }
}

// ── Standalone path-traversal guard (mirrors query impl) ──────────────────

fn deepest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return path.to_path_buf(),
        }
    }
    current
}

fn resolve_workspace_root(
    work_dir: &Path,
    workspace_id: Option<&str>,
) -> Result<PathBuf, WorkspaceError> {
    let ws_id = match workspace_id {
        Some(id) if !id.is_empty() && id != "__agent_home__" => id,
        _ => return Ok(work_dir.to_path_buf()),
    };

    let config_path = work_dir.join("config").join("agent_workspaces.json");
    let content = std::fs::read_to_string(&config_path).map_err(|_| {
        WorkspaceError::WorkspaceNotFound(ws_id.to_string())
    })?;
    let val: serde_json::Value =
        serde_json::from_str(&content).map_err(WorkspaceError::Json)?;

    if let Some(dirs) = val.get("additional_dirs").and_then(|v| v.as_array()) {
        for dir in dirs {
            if let Some(path) = dir
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|id| *id == ws_id)
                .and_then(|_| dir.get("path").and_then(|v| v.as_str()))
            {
                return Ok(PathBuf::from(path));
            }
        }
    }
    Err(WorkspaceError::WorkspaceNotFound(ws_id.to_string()))
}

fn resolve_within_static(
    work_dir: &Path,
    workspace_id: Option<&str>,
    requested_path: &str,
) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
    let workspace_root = resolve_workspace_root(work_dir, workspace_id)?;
    let abs_path = if requested_path.is_empty() {
        workspace_root.clone()
    } else {
        workspace_root.join(requested_path)
    };
    let canonical_root = std::fs::canonicalize(&workspace_root).map_err(|e| {
        WorkspaceError::Io(std::io::Error::other(format!(
            "workspace root not accessible: {}",
            e
        )))
    })?;
    let check_path = deepest_existing_ancestor(&abs_path);
    let canonical_check = std::fs::canonicalize(&check_path).unwrap_or(check_path);
    if !canonical_check.starts_with(&canonical_root) {
        return Err(WorkspaceError::InvalidPath(
            "path traversal detected".to_string(),
        ));
    }
    let rel_path = abs_path
        .strip_prefix(&workspace_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((canonical_root, abs_path, rel_path))
}

// ── Trait impl ────────────────────────────────────��────────────────────────

#[async_trait]
impl WorkspaceMutationService for RuntimeWorkspaceMutationService {
    async fn create_workspace(
        &self,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config();
        if cfg.additional_dirs.iter().any(|d| {
            d.get("id").and_then(|v| v.as_str()) == Some(&entry.id)
        }) {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("workspace id already exists: {}", entry.id),
            });
        }
        let new_entry = serde_json::json!({
            "id": entry.id,
            "path": entry.path,
            "access": entry.access,
            "prompt_file": entry.prompt_file,
            "last_active": entry.last_active.unwrap_or(false),
        });
        cfg.additional_dirs.push(new_entry.clone());
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(new_entry),
        })
    }

    async fn update_workspace(
        &self,
        ws_id: &str,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config();
        let target = cfg
            .additional_dirs
            .iter_mut()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(ws_id))
            .ok_or_else(|| WorkspaceError::WorkspaceNotFound(ws_id.to_string()))?;
        target["id"] = serde_json::json!(entry.id);
        target["path"] = serde_json::json!(entry.path);
        target["access"] = serde_json::json!(entry.access);
        if let Some(pf) = &entry.prompt_file {
            target["prompt_file"] = serde_json::json!(pf);
        }
        if let Some(la) = entry.last_active {
            target["last_active"] = serde_json::json!(la);
        }
        let updated = target.clone();
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(updated),
        })
    }

    async fn set_prompt_file(
        &self,
        ws_id: &str,
        body: PromptFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config();
        let target = cfg
            .additional_dirs
            .iter_mut()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(ws_id))
            .ok_or_else(|| WorkspaceError::WorkspaceNotFound(ws_id.to_string()))?;
        target["prompt_file"] = serde_json::json!(body.prompt_file);
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: None,
        })
    }

    async fn delete_workspace(
        &self,
        ws_id: &str,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config();
        let initial_len = cfg.additional_dirs.len();
        cfg.additional_dirs
            .retain(|d| d.get("id").and_then(|v| v.as_str()) != Some(ws_id));
        if cfg.additional_dirs.len() == initial_len {
            return Err(WorkspaceError::WorkspaceNotFound(ws_id.to_string()));
        }
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: None,
        })
    }

    async fn create_file(
        &self,
        body: CreateFileBody,
        query_ws: Option<&str>,
        query_path: Option<&str>,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            query_path,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) =
            self.resolve_within(workspace_id.as_deref(), &path)?;

        if abs_path.exists() && !body.overwrite {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("file already exists: {}", rel_path),
            });
        }
        if let Some(parent) = abs_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorkspaceError::Persist(format!("failed to create parent directory: {}", e))
            })?;
        }
        std::fs::write(&abs_path, body.content.as_bytes())
            .map_err(|e| WorkspaceError::Persist(format!("failed to write file: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"created": true, "path": rel_path})),
        })
    }

    async fn write_file(
        &self,
        query: FilePathQuery,
        body: crate::usecases::workspace_mutation::WriteFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (_root, abs_path, rel_path) =
            self.resolve_within(query.workspace_id.as_deref(), &query.path)?;

        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "file does not exist: {}",
                rel_path
            )));
        }
        if abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a directory; use DELETE /workspaces/dir instead: {}",
                rel_path
            )));
        }
        std::fs::write(&abs_path, body.content.as_bytes())
            .map_err(|e| WorkspaceError::Persist(format!("failed to write file: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"written": true, "path": rel_path})),
        })
    }

    async fn delete_file(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) =
            self.resolve_within(workspace_id.as_deref(), &path)?;

        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "file does not exist: {}",
                rel_path
            )));
        }
        if abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a directory; use DELETE /workspaces/dir instead: {}",
                rel_path
            )));
        }
        std::fs::remove_file(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to delete file: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"deleted": true, "path": rel_path})),
        })
    }

    async fn create_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) =
            self.resolve_within(workspace_id.as_deref(), &path)?;

        std::fs::create_dir_all(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to create directory: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"created": true, "path": rel_path})),
        })
    }

    async fn delete_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) =
            self.resolve_within(workspace_id.as_deref(), &path)?;

        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "directory does not exist: {}",
                rel_path
            )));
        }
        if !abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a file; use DELETE /workspaces/file instead: {}",
                rel_path
            )));
        }
        std::fs::remove_dir_all(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to delete directory: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"deleted": true, "path": rel_path})),
        })
    }
}

// ── FilePathQuery note ─────────────────────────────────────────────────────
//
// `write_file` in this trait accepts `FilePathQuery` (defined in
// `workspace_mutation.rs`). The query service has its own
// `FilePathQuery` with identical fields — keeping them separate avoids
// a circular trait ↔ impl dependency between the two services. The HTTP
// layer maps a single `Query<FilePathQuery>` extractor into whichever
// type the handler needs.
//
// We intentionally do not add a `From` impl between the two: callers
// pass `FilePathQuery` directly via the HTTP layer's `Json` body /
// `Query` extractor, and the runtime never constructs one from the other.