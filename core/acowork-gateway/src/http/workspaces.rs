//! Workspace directory management API
//!
//! Manages additional directories that agents can access beyond their workspace.
//!
//! **ADR-009 (v2)**: Gateway is pure pass-through for workspace config.
//! No persistence to disk. Workspace config is maintained by Agent Runtime
//! (in `agent_workspaces.json`). Gateway caches the config in `RunningAgentInfo`
//! (in-memory only, cleared on disconnect) to serve HTTP API requests.
//! CRUD operations serialize the full config → push `WorkspaceConfigUpdate` via IPC.
//! Agent must be running (HTTP API returns 409 if not).

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use uuid::Uuid;

use crate::http::routes::{ApiError, AppState};

/// Workspace directory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDir {
    pub id: String,
    pub path: String,
    pub alias: Option<String>,
    pub access: AccessLevel,
    pub added_at: String,
    /// Deprecated: replaced by session-level workspace selection.
    /// Renamed from `is_current` for backward-compatible JSON reading.
    /// Frontend should read `sessionWorkspaceMap` instead.
    #[serde(default, alias = "is_current")]
    pub last_active: bool,
    /// Cumulative selection count for context ranking
    #[serde(default)]
    pub select_count: u32,
    /// Last selection timestamp (RFC3339), None if never selected
    #[serde(default)]
    pub last_selected_at: Option<String>,
    /// Prompt file to inject into system prompt (e.g. "CLAUDE.md", "AGENTS.md").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
}

/// Access level for workspace directories
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum AccessLevel {
    ReadOnly,
    ReadWrite,
}

/// Workspace configuration file structure (for JSON serialization)
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceConfig {
    pub version: String,
    pub additional_dirs: Vec<WorkspaceDir>,
}

/// Request to add a workspace directory
#[derive(Debug, Deserialize)]
pub struct AddWorkspaceRequest {
    pub path: String,
    pub alias: Option<String>,
    pub access: AccessLevel,
}

/// Request to set the current (active) workspace
#[derive(Debug, Deserialize)]
pub struct SetCurrentWorkspaceRequest {
    pub workspace_id: String,
}

/// Query parameters for set_current_workspace (optional session_id for per-session selection).
#[derive(Debug, Deserialize, Default)]
pub struct SetCurrentWorkspaceQuery {
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Request to update a workspace directory
#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub access: Option<AccessLevel>,
    pub alias: Option<String>,
}

/// Request to set/unset prompt file for a workspace
#[derive(Debug, Deserialize)]
pub struct SetPromptFileRequest {
    pub prompt_file: Option<String>,
}

/// List of workspace directories
#[derive(Debug, Serialize)]
pub struct WorkspaceListResponse {
    pub agent_id: String,
    pub workspaces: Vec<WorkspaceDir>,
}

/// Helper: get workspace config from RunningAgentInfo cache.
/// Returns None if agent not running.
async fn get_cached_config(state: &AppState, agent_id: &str) -> Option<WorkspaceConfig> {
    let gw = state.gateway_state.read().await;
    let info = gw.running_agents.get(agent_id)?;
    let json = info.workspace_config_json.as_ref()?;
    serde_json::from_str(json).ok()
}

/// Helper: push WorkspaceConfigUpdate to Runtime and update the cache.
///
/// ADR-009: IPC push is synchronous — we await the result before updating
/// the in-memory cache. This avoids TOCTOU where the cache shows a config
/// that Runtime never received (e.g. channel closed mid-push).
async fn push_and_cache(
    state: &AppState,
    agent_id: &str,
    config: &WorkspaceConfig,
) -> Result<(), String> {
    let config_json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    // Push to Runtime via IPC first — only update cache on success
    if let Some(ref session_mgr) = state.session_mgr {
        let push_tx = {
            let mgr = session_mgr.lock().await;
            mgr.find_by_agent_id(agent_id)
                .and_then(|(_, session)| session.push_sender().cloned())
        };
        if let Some(push_tx) = push_tx {
            let push_msg = acowork_core::protocol::GatewayResponse::WorkspaceConfigUpdate {
                config_json: config_json.clone(),
            };
            if push_tx.send(push_msg).await.is_err() {
                tracing::warn!(
                    "Failed to push WorkspaceConfigUpdate to agent={} (channel closed)",
                    agent_id
                );
                return Err(format!(
                    "Agent {} is not reachable (IPC channel closed), cannot update workspace",
                    agent_id
                ));
            }
            tracing::info!("Pushed WorkspaceConfigUpdate to agent={}", agent_id);
        } else {
            // Agent has no active IPC session — cannot update workspace
            return Err(format!(
                "Agent {} has no active IPC session, cannot update workspace",
                agent_id
            ));
        }
    } else {
        return Err("No session manager available".to_string());
    }

    // IPC push succeeded — now update in-memory cache
    {
        let mut gw = state.gateway_state.write().await;
        if let Some(info) = gw.running_agents.get_mut(agent_id) {
            info.workspace_config_json = Some(config_json);
        }
    }

    Ok(())
}

// ─── Handlers ────────────────────────────────────────────────────────────

/// `GET /api/agents/{agent_id}/workspaces` — list workspace directories for an agent
pub async fn list_workspaces(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<WorkspaceListResponse>, (StatusCode, Json<ApiError>)> {
    // ADR-009 v2: Read from RunningAgentInfo in-memory cache
    // If agent is running → return its workspace config
    // If agent exists but not running → return empty list (per ADR-009)
    // If agent doesn't exist → return 404
    let config = get_cached_config(&state, &agent_id).await;

    match config {
        Some(cfg) => Ok(Json(WorkspaceListResponse {
            agent_id,
            workspaces: cfg.additional_dirs,
        })),
        None => {
            // Check if agent exists (installed but not running)
            let gw = state.gateway_state.read().await;
            if gw.installed_agents.contains_key(&agent_id) {
                // Agent exists but not running → empty list per ADR-009
                Ok(Json(WorkspaceListResponse {
                    agent_id,
                    workspaces: vec![],
                }))
            } else {
                Err(ApiError::not_found("Agent not found"))
            }
        }
    }
}

/// `POST /api/agents/{agent_id}/workspaces` — add a workspace directory
pub async fn add_workspace(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceDir>), (StatusCode, Json<ApiError>)> {
    // Validate path exists and is a directory
    if !StdPath::new(&req.path).is_dir() {
        return Err(ApiError::bad_request(&format!(
            "Directory not found: {}",
            req.path
        )));
    }

    // Load current config from cache
    let mut config = get_cached_config(&state, &agent_id)
        .await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot add workspace"))?;

    // Check for duplicate path
    if config.additional_dirs.iter().any(|d| d.path == req.path) {
        return Err(ApiError::bad_request(
            "Directory already exists in workspace list",
        ));
    }

    // Create new entry
    let new_dir = WorkspaceDir {
        id: format!("ws-{}", &Uuid::new_v4().to_string().replace("-", "")[..12]),
        path: req.path.clone(),
        alias: req.alias,
        access: req.access,
        added_at: chrono::Utc::now().to_rfc3339(),
        last_active: false,
        select_count: 0,
        last_selected_at: None,
        prompt_file: None,
    };

    let result = new_dir.clone();
    config.additional_dirs.push(new_dir);

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// `PUT /api/agents/{agent_id}/workspaces/{ws_id}` — update a workspace directory
pub async fn update_workspace(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
    Json(req): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceDir>, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id)
        .await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot update workspace"))?;

    // Find and update directory
    let dir = config
        .additional_dirs
        .iter_mut()
        .find(|d| d.id == ws_id)
        .ok_or_else(|| ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)))?;

    if let Some(access) = req.access {
        dir.access = access;
    }
    if let Some(alias) = req.alias {
        dir.alias = Some(alias);
    }

    let updated = dir.clone();

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(updated))
}

/// `PUT /api/agents/{agent_id}/workspaces/{ws_id}/prompt-file` — set/unset prompt file for a workspace
pub async fn set_prompt_file(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
    Json(req): Json<SetPromptFileRequest>,
) -> Result<Json<WorkspaceDir>, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id)
        .await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot update workspace"))?;

    // Find and update directory
    let dir = config
        .additional_dirs
        .iter_mut()
        .find(|d| d.id == ws_id)
        .ok_or_else(|| ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)))?;

    dir.prompt_file = req.prompt_file;
    let updated = dir.clone();

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(updated))
}

/// `PUT /api/agents/{agent_id}/workspaces/current` — set the current (active) workspace
///
/// Optional query param `session_id` enables per-session workspace selection.
/// When provided, Gateway also sends `SetSessionWorkspace` IPC to the Runtime
/// in addition to the `WorkspaceConfigUpdate` (which updates list stats).
pub async fn set_current_workspace(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<SetCurrentWorkspaceQuery>,
    Json(req): Json<SetCurrentWorkspaceRequest>,
) -> Result<Json<WorkspaceListResponse>, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id)
        .await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot set workspace"))?;

    // Validate workspace: either "__agent_home__" or an existing workspace ID
    let is_agent_home = req.workspace_id == "__agent_home__";
    if !is_agent_home
        && !config
            .additional_dirs
            .iter()
            .any(|d| d.id == req.workspace_id)
    {
        return Err(ApiError::not_found(&format!(
            "Workspace directory not found: {}",
            req.workspace_id
        )));
    }

    // When session_id is provided, push SetSessionWorkspace to Runtime
    if let Some(ref session_id) = query.session_id
        && let Some(ref session_mgr) = state.session_mgr {
            let push_tx = {
                let mgr = session_mgr.lock().await;
                mgr.find_by_agent_id(&agent_id)
                    .and_then(|(_, session)| session.push_sender().cloned())
            };
            if let Some(push_tx) = push_tx {
                let push_msg = acowork_core::protocol::GatewayResponse::SetSessionWorkspace {
                    session_id: session_id.clone(),
                    workspace_id: req.workspace_id.clone(),
                };
                if push_tx.send(push_msg).await.is_err() {
                    tracing::warn!(
                        agent_id = %agent_id,
                        session_id = %session_id,
                        "Failed to push SetSessionWorkspace (channel closed)"
                    );
                } else {
                    tracing::info!(
                        agent_id = %agent_id,
                        session_id = %session_id,
                        workspace_id = %req.workspace_id,
                        "Pushed SetSessionWorkspace to Runtime"
                    );
                }
            }
        }

    // Update select_count and last_selected_at for the selected workspace (if it's a user workspace)
    if !is_agent_home {
        let now = chrono::Utc::now().to_rfc3339();
        for dir in &mut config.additional_dirs {
            if dir.id == req.workspace_id {
                dir.last_active = true;
                dir.select_count += 1;
                dir.last_selected_at = Some(now.clone());
            } else {
                dir.last_active = false;
            }
        }
    }

    // Push WorkspaceConfigUpdate to Runtime (updates list stats + cache)
    push_and_cache(&state, &agent_id, &config)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(WorkspaceListResponse {
        agent_id,
        workspaces: config.additional_dirs,
    }))
}

/// `DELETE /api/agents/{agent_id}/workspaces/{ws_id}` — remove a workspace directory
pub async fn delete_workspace(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id)
        .await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot delete workspace"))?;

    // Check if exists
    if !config.additional_dirs.iter().any(|d| d.id == ws_id) {
        return Err(ApiError::not_found(&format!(
            "Workspace directory not found: {}",
            ws_id
        )));
    }

    // Remove directory
    config.additional_dirs.retain(|d| d.id != ws_id);

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(StatusCode::NO_CONTENT)
}

// ─── File Tree Explorer API ─────────────────────────────────────────────

/// A single entry in a directory listing (file or subdirectory)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
    /// File or directory name
    pub name: String,
    /// "file" or "directory"
    #[serde(rename = "type")]
    pub entry_type: String,
    /// File size in bytes (None for directories)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Last modified timestamp (RFC3339, None if unavailable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Number of direct children (only for directories, used for showing expansion arrow)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<usize>,
}

/// Query parameters for the tree endpoint
#[derive(Debug, Deserialize, Default)]
pub struct TreeQuery {
    /// Relative path within the workspace root (empty or "." = root)
    #[serde(default)]
    pub path: Option<String>,
    /// Workspace ID to browse. "__agent_home__" or empty = agent home directory.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// Response for the tree endpoint
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeResponse {
    /// Absolute path of the workspace root
    pub root: String,
    /// Relative path that was listed
    pub path: String,
    /// Directory entries (directories first, then files, both alphabetical)
    pub entries: Vec<TreeEntry>,
}

/// Resolve the absolute directory path for a tree request, ensuring it stays
/// within the allowed workspace root. Returns `(root, abs_path, rel_path)`.
///
/// For paths that don't yet exist on disk (e.g. creating a new file), the
/// canonicalization is skipped and containment is verified by checking for
/// parent-directory traversal (`..`) and absolute-path components.
fn resolve_tree_path(
    root: &str,
    requested_path: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf, String), String> {
    let root = std::path::Path::new(root);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("Cannot resolve workspace root: {}", e))?;

    let rel = requested_path
        .trim_start_matches("./")
        .trim_start_matches("/");
    let abs = if rel.is_empty() || rel == "." {
        canonical_root.clone()
    } else {
        let candidate = canonical_root.join(rel);
        // Prevent path traversal: the canonicalized path must start with root
        match candidate.canonicalize() {
            Ok(canonical_candidate) => {
                if !canonical_candidate.starts_with(&canonical_root) {
                    return Err("Path is outside the workspace root".to_string());
                }
                canonical_candidate
            }
            Err(_) => {
                // Path doesn't exist on disk yet (e.g. creating a new file/dir).
                // Validate containment without requiring the path to exist.
                let rel_path = std::path::Path::new(rel);
                // Reject `..` components that would escape the workspace
                if rel_path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                {
                    return Err("Path traversal not allowed".to_string());
                }
                // Reject absolute-looking paths: on Windows `root.join("C:\\x")`
                // replaces the entire path, bypassing the workspace root.
                if rel_path.has_root() {
                    return Err("Absolute paths not allowed".to_string());
                }
                candidate
            }
        }
    };

    let rel_path = abs
        .strip_prefix(&canonical_root)
        .unwrap_or(std::path::Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");

    Ok((canonical_root, abs, rel_path))
}

/// `GET /api/agents/{agent_id}/workspaces/tree` — list directory contents
///
/// Returns a flat list of entries for a single directory level (depth=1).
/// Security: only allows browsing within the workspace root directory.
/// The `path` query parameter is relative to the workspace root.
/// The `workspace_id` parameter selects which workspace to browse:
///   - empty or `"__agent_home__"` → agent installation directory
///   - a workspace ID (e.g. `"ws-abc123"`) → that workspace's path
pub async fn list_tree(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<TreeResponse>, (StatusCode, Json<ApiError>)> {
    // Determine the workspace root based on workspace_id
    let workspace_root = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .running_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found("Agent not running — cannot browse workspace"))?;

        let ws_id = query.workspace_id.as_deref().unwrap_or("");

        if ws_id.is_empty() || ws_id == "__agent_home__" {
            // Agent home directory
            info.workspace.clone()
        } else {
            // Look up workspace path from cached config
            let config = info
                .workspace_config_json
                .as_ref()
                .and_then(|json| serde_json::from_str::<WorkspaceConfig>(json).ok());
            match config {
                Some(cfg) => cfg
                    .additional_dirs
                    .iter()
                    .find(|d| d.id == ws_id)
                    .map(|d| d.path.clone())
                    .ok_or_else(|| {
                        ApiError::not_found(&format!("Workspace directory not found: {}", ws_id))
                    })?,
                None => {
                    return Err(ApiError::not_found(
                        "Agent workspace config not available yet",
                    ));
                }
            }
        }
    };

    let requested_path = query.path.as_deref().unwrap_or("").to_string();
    let (canonical_root, abs_path, rel_path) = resolve_tree_path(&workspace_root, &requested_path)
        .map_err(|e| ApiError::bad_request(&e))?;

    // Read directory entries
    let read_dir = match std::fs::read_dir(&abs_path) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(ApiError::internal(&format!(
                "Failed to read directory: {}",
                e
            )));
        }
    };

    // Strip the Windows extended-length path prefix (\\?\) that canonicalize()
    // produces on Windows. This prefix is not valid in file URIs and breaks
    // LSP document URIs (e.g. "file:////?/C:/..." instead of "file:///C:/...").
    let canonical_str = canonical_root.to_string_lossy();
    let stripped = canonical_str
        .strip_prefix(r"\\?\")
        .unwrap_or(canonical_str.as_ref());
    let root_str = stripped.replace('\\', "/");
    let mut dirs: Vec<TreeEntry> = Vec::new();
    let mut files: Vec<TreeEntry> = Vec::new();

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // Skip unreadable entries
        };

        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files/dirs (starting with '.')
        if name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());

        if is_dir {
            // Count children for the expansion indicator
            let children_count = std::fs::read_dir(entry.path())
                .ok()
                .map(|rd| {
                    rd.filter(|e| {
                        e.as_ref()
                            .map(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .unwrap_or(false)
                    })
                    .count()
                })
                .unwrap_or(0);

            dirs.push(TreeEntry {
                name,
                entry_type: "directory".to_string(),
                size: None,
                modified: metadata.and_then(|m| {
                    m.modified().ok().and_then(|t| {
                        t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| {
                                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default()
                            })
                    })
                }),
                children_count: Some(children_count),
            });
        } else {
            files.push(TreeEntry {
                name,
                entry_type: "file".to_string(),
                size: metadata.as_ref().map(|m| m.len()),
                modified: metadata.and_then(|m| {
                    m.modified().ok().and_then(|t| {
                        t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| {
                                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default()
                            })
                    })
                }),
                children_count: None,
            });
        }
    }

    // Sort: directories first, then files — both alphabetical (case-insensitive)
    dirs.sort_by_key(|a| a.name.to_lowercase());
    files.sort_by_key(|a| a.name.to_lowercase());

    let mut entries = dirs;
    entries.append(&mut files);

    Ok(Json(TreeResponse {
        root: root_str,
        path: rel_path,
        entries,
    }))
}

// ─── File Content API ────────────────────────────────────────────────────

/// Maximum file size for read/write operations (5 MB)
const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024;

/// Text-based MIME types allowed for file editing
fn detect_mime(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "rs" => Some("text/x-rust"),
        "ts" | "tsx" => Some("text/typescript"),
        "js" | "jsx" => Some("text/javascript"),
        "json" => Some("application/json"),
        "toml" => Some("application/toml"),
        "yaml" | "yml" => Some("text/yaml"),
        "md" | "markdown" => Some("text/markdown"),
        "html" | "htm" => Some("text/html"),
        "css" | "scss" | "less" => Some("text/css"),
        "xml" => Some("text/xml"),
        "sh" | "bash" | "zsh" => Some("text/x-shellscript"),
        "ps1" | "psm1" | "psd1" => Some("text/x-powershell"),
        "bat" | "cmd" => Some("text/x-bat"),
        "py" => Some("text/x-python"),
        "rb" => Some("text/x-ruby"),
        "go" => Some("text/x-go"),
        "java" => Some("text/x-java"),
        "c" | "h" => Some("text/x-c"),
        "cpp" | "cc" | "cxx" | "hpp" => Some("text/x-cpp"),
        "cs" => Some("text/x-csharp"),
        "swift" => Some("text/x-swift"),
        "kt" | "kts" => Some("text/x-kotlin"),
        "sql" => Some("text/x-sql"),
        "graphql" | "gql" => Some("text/x-graphql"),
        "dockerfile" => Some("text/x-dockerfile"),
        "env" | "ini" | "cfg" | "conf" => Some("text/plain"),
        "txt" | "log" | "csv" => Some("text/plain"),
        "gitignore" | "editorconfig" => Some("text/plain"),
        // Image types
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

/// Query parameters for file read/write
#[derive(Debug, Deserialize, Default)]
pub struct FileQuery {
    /// Relative file path within the workspace
    pub path: Option<String>,
    /// Workspace ID. "__agent_home__" or empty = agent home directory
    pub workspace_id: Option<String>,
}

/// Response for file read
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileResponse {
    pub content: String,
    pub size: u64,
    pub mime_type: String,
}

/// Request body for file write
#[derive(Debug, Deserialize)]
pub struct WriteFileRequest {
    pub content: String,
}

/// Request body for creating a new file/directory
#[derive(Debug, Deserialize)]
pub struct CreateFileRequest {
    /// Relative path of the new file within the workspace
    pub path: String,
}

/// Response for file/directory creation
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFileResponse {
    pub ok: bool,
    pub path: String,
}

/// Response for file write
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFileResponse {
    pub ok: bool,
    pub size: u64,
}

/// Request body for copy operation
#[derive(Debug, Deserialize)]
pub struct CopyRequest {
    /// Relative path of the source file/directory
    pub source: String,
    /// Relative path of the destination
    pub dest: String,
}

/// Request body for delete operation
#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    /// Relative path to delete
    pub path: String,
}

/// Resolve workspace root path for a given agent + workspace_id.
/// Shared between tree and file APIs.
async fn resolve_workspace_root(
    state: &AppState,
    agent_id: &str,
    workspace_id: Option<&str>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let info = gw
        .running_agents
        .get(agent_id)
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot access workspace"))?;

    let ws_id = workspace_id.unwrap_or("");
    if ws_id.is_empty() || ws_id == "__agent_home__" {
        Ok(info.workspace.clone())
    } else {
        let config = info
            .workspace_config_json
            .as_ref()
            .and_then(|json| serde_json::from_str::<WorkspaceConfig>(json).ok());
        match config {
            Some(cfg) => cfg
                .additional_dirs
                .iter()
                .find(|d| d.id == ws_id)
                .map(|d| d.path.clone())
                .ok_or_else(|| {
                    ApiError::not_found(&format!("Workspace directory not found: {}", ws_id))
                }),
            None => Err(ApiError::not_found(
                "Agent workspace config not available yet",
            )),
        }
    }
}

/// `GET /api/agents/{agent_id}/workspaces/file` — read a file's content
pub async fn read_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileResponse>, (StatusCode, Json<ApiError>)> {
    let file_rel_path = query.path.as_deref().unwrap_or("");
    if file_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    // Verify it's a file
    if !abs_path.is_file() {
        return Err(ApiError::bad_request("Path is not a file"));
    }

    // Check file size
    let metadata = std::fs::metadata(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Cannot read metadata: {}", e)))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError {
                error: format!(
                    "File too large ({} bytes, max {} bytes)",
                    metadata.len(),
                    MAX_FILE_SIZE
                ),
                code: 413,
            }),
        ));
    }

    // Detect MIME type
    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime_type = detect_mime(ext).unwrap_or("text/plain").to_string();

    // Read content: binary files (images, etc.) are base64-encoded;
    // text files are read as UTF-8 strings.
    let content = if mime_type.starts_with("image/") {
        let bytes = std::fs::read(&abs_path)
            .map_err(|e| ApiError::internal(&format!("Failed to read file: {}", e)))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    } else {
        std::fs::read_to_string(&abs_path)
            .map_err(|e| ApiError::internal(&format!("Failed to read file: {}", e)))?
    };

    Ok(Json(FileResponse {
        content,
        size: metadata.len(),
        mime_type,
    }))
}

/// `GET /api/agents/{agent_id}/workspaces/file-raw` — read a file's raw bytes
///
/// Returns the file content directly (not wrapped in JSON) with the correct
/// Content-Type header. This is used for iframe src URLs (HTML preview, etc.)
/// where a proper HTTP origin is required for scripts to load without CORS issues.
pub async fn read_raw_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<(StatusCode, [(String, String); 1], axum::body::Body), (StatusCode, Json<ApiError>)> {
    let file_rel_path = query.path.as_deref().unwrap_or("");
    if file_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    if !abs_path.is_file() {
        return Err(ApiError::bad_request("Path is not a file"));
    }

    let metadata = std::fs::metadata(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Cannot read metadata: {}", e)))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError {
                error: format!(
                    "File too large ({} bytes, max {} bytes)",
                    metadata.len(),
                    MAX_FILE_SIZE
                ),
                code: 413,
            }),
        ));
    }

    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime_type = detect_mime(ext).unwrap_or("text/plain").to_string();

    let bytes = std::fs::read(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to read file: {}", e)))?;

    Ok((
        StatusCode::OK,
        [("Content-Type".to_string(), mime_type)],
        axum::body::Body::from(bytes),
    ))
}

/// Response type for streaming file responses.
type StreamFileResponse =
    Result<(StatusCode, [(&'static str, String); 2], axum::body::Body), (StatusCode, Json<ApiError>)>;

/// Serve a raw file from a resolved workspace root with MIME and containment checks.
fn serve_workspace_file_from_root(
    workspace_root: String,
    file_rel_path: &str,
) -> StreamFileResponse {
    if file_rel_path.is_empty() || file_rel_path == "/" {
        return Err(ApiError::bad_request("Missing file path"));
    }

    let file_rel_path = file_rel_path.trim_start_matches('/');
    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    if !abs_path.is_file() {
        return Err(ApiError::not_found(&format!("File not found: {}", file_rel_path)));
    }

    let metadata = std::fs::metadata(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Cannot read metadata: {}", e)))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError {
                error: format!(
                    "File too large ({} bytes, max {} bytes)",
                    metadata.len(),
                    MAX_FILE_SIZE
                ),
                code: 413,
            }),
        ));
    }

    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime_type = detect_mime(ext).unwrap_or("application/octet-stream").to_string();
    let bytes = std::fs::read(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to read file: {}", e)))?;

    Ok((
        StatusCode::OK,
        [("Content-Type", mime_type), ("Access-Control-Allow-Origin", "*".to_string())],
        axum::body::Body::from(bytes),
    ))
}

/// `GET /ws-files/{agent_id}/{*path}` — serve an agent-home file as a static asset
///
/// Legacy endpoint kept for compatibility. New HTML preview code should use
/// `/workspace-files/{agent_id}/{workspace_id}/{*path}` so additional
/// workspace directories resolve correctly.
pub async fn serve_ws_file(
    State(state): State<AppState>,
    Path((agent_id, file_rel_path)): Path<(String, String)>,
) -> StreamFileResponse {
    let workspace_root = resolve_workspace_root(&state, &agent_id, None).await?;
    serve_workspace_file_from_root(workspace_root, &file_rel_path)
}

/// `GET /workspace-files/{agent_id}/{workspace_id}/{*path}` — serve a workspace file as a static asset
///
/// This endpoint is used by HTML preview so sub-resources are resolved against
/// the same workspace as the HTML file being previewed.
pub async fn serve_workspace_ws_file(
    State(state): State<AppState>,
    Path((agent_id, workspace_id, file_rel_path)): Path<(String, String, String)>,
) -> StreamFileResponse {
    let workspace_root = resolve_workspace_root(&state, &agent_id, Some(&workspace_id)).await?;
    serve_workspace_file_from_root(workspace_root, &file_rel_path)
}

/// `PUT /api/agents/{agent_id}/workspaces/file` — write content to a file
pub async fn write_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<WriteFileRequest>,
) -> Result<Json<WriteFileResponse>, (StatusCode, Json<ApiError>)> {
    let file_rel_path = query.path.as_deref().unwrap_or("");
    if file_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    // Check content size
    let content_bytes = req.content.len() as u64;
    if content_bytes > MAX_FILE_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ApiError {
                error: format!(
                    "Content too large ({} bytes, max {} bytes)",
                    content_bytes, MAX_FILE_SIZE
                ),
                code: 413,
            }),
        ));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    // Verify parent directory exists (allow creating new files)
    if let Some(parent) = abs_path.parent()
        && !parent.is_dir() {
            return Err(ApiError::bad_request(&format!(
                "Parent directory does not exist: {}",
                parent.display()
            )));
        }

    // Check write access for read-only workspaces
    {
        let gw = state.gateway_state.read().await;
        if let Some(info) = gw.running_agents.get(&agent_id) {
            let ws_id = query.workspace_id.as_deref().unwrap_or("");
            if !ws_id.is_empty() && ws_id != "__agent_home__"
                && let Some(config) = info
                    .workspace_config_json
                    .as_ref()
                    .and_then(|json| serde_json::from_str::<WorkspaceConfig>(json).ok())
                    && let Some(dir) = config.additional_dirs.iter().find(|d| d.id == ws_id)
                        && dir.access == AccessLevel::ReadOnly {
                            return Err(ApiError::bad_request(
                                "Workspace is read-only, cannot write files",
                            ));
                        }
        }
    }

    // Write content
    std::fs::write(&abs_path, &req.content)
        .map_err(|e| ApiError::internal(&format!("Failed to write file: {}", e)))?;

    Ok(Json(WriteFileResponse {
        ok: true,
        size: content_bytes,
    }))
}

/// `POST /api/agents/{agent_id}/workspaces/file` — create an empty new file
pub async fn create_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<CreateFileRequest>,
) -> Result<(StatusCode, Json<CreateFileResponse>), (StatusCode, Json<ApiError>)> {
    let file_rel_path = if !req.path.is_empty() {
        req.path.as_str()
    } else {
        query.path.as_deref().unwrap_or("")
    };
    if file_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    // Create parent directories if needed
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ApiError::internal(&format!("Failed to create parent directory: {}", e))
        })?;
    }

    // Create empty file (error if already exists)
    std::fs::File::create_new(&abs_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            ApiError::bad_request(&format!("File already exists: {}", file_rel_path))
        } else {
            ApiError::internal(&format!("Failed to create file: {}", e))
        }
    })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateFileResponse {
            ok: true,
            path: file_rel_path.to_string(),
        }),
    ))
}

/// `POST /api/agents/{agent_id}/workspaces/dir` — create a new directory
pub async fn create_dir(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<CreateFileRequest>,
) -> Result<(StatusCode, Json<CreateFileResponse>), (StatusCode, Json<ApiError>)> {
    let dir_rel_path = if !req.path.is_empty() {
        req.path.as_str()
    } else {
        query.path.as_deref().unwrap_or("")
    };
    if dir_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, dir_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    std::fs::create_dir_all(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to create directory: {}", e)))?;

    Ok((
        StatusCode::CREATED,
        Json(CreateFileResponse {
            ok: true,
            path: dir_rel_path.to_string(),
        }),
    ))
}

// ─── Delete / Copy API ─────────────────────────────────────────────────

/// `DELETE /api/agents/{agent_id}/workspaces/file` — delete a file
pub async fn delete_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<CreateFileResponse>, (StatusCode, Json<ApiError>)> {
    let file_rel_path = if !req.path.is_empty() {
        req.path.as_str()
    } else {
        query.path.as_deref().unwrap_or("")
    };
    if file_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, file_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    let metadata = std::fs::metadata(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to read file metadata: {}", e)))?;

    if metadata.is_dir() {
        return Err(ApiError::bad_request(
            "Path is a directory, use /dir endpoint",
        ));
    }

    std::fs::remove_file(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to delete file: {}", e)))?;

    Ok(Json(CreateFileResponse {
        ok: true,
        path: file_rel_path.to_string(),
    }))
}

/// `DELETE /api/agents/{agent_id}/workspaces/dir` — delete a directory recursively
pub async fn delete_dir(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<CreateFileResponse>, (StatusCode, Json<ApiError>)> {
    let dir_rel_path = if !req.path.is_empty() {
        req.path.as_str()
    } else {
        query.path.as_deref().unwrap_or("")
    };
    if dir_rel_path.is_empty() {
        return Err(ApiError::bad_request("Missing required 'path' parameter"));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_path, _rel_path) =
        resolve_tree_path(&workspace_root, dir_rel_path).map_err(|e| ApiError::bad_request(&e))?;

    let metadata = std::fs::metadata(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to read directory metadata: {}", e)))?;

    if !metadata.is_dir() {
        return Err(ApiError::bad_request("Path is a file, use /file endpoint"));
    }

    std::fs::remove_dir_all(&abs_path)
        .map_err(|e| ApiError::internal(&format!("Failed to delete directory: {}", e)))?;

    Ok(Json(CreateFileResponse {
        ok: true,
        path: dir_rel_path.to_string(),
    }))
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("Failed to create destination directory: {}", e))?;

    let entries =
        std::fs::read_dir(src).map_err(|e| format!("Failed to read source directory: {}", e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("Failed to copy file: {}", e))?;
        }
    }

    Ok(())
}

/// `POST /api/agents/{agent_id}/workspaces/copy` — copy a file or directory
pub async fn copy_item(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<CopyRequest>,
) -> Result<(StatusCode, Json<CreateFileResponse>), (StatusCode, Json<ApiError>)> {
    if req.source.is_empty() || req.dest.is_empty() {
        return Err(ApiError::bad_request(
            "Missing required 'source' or 'dest' parameter",
        ));
    }

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let (_canonical_root, abs_src, _rel_src) =
        resolve_tree_path(&workspace_root, &req.source).map_err(|e| ApiError::bad_request(&e))?;

    let (_canonical_root, abs_dest, _rel_dest) =
        resolve_tree_path(&workspace_root, &req.dest).map_err(|e| ApiError::bad_request(&e))?;

    if abs_dest.exists() {
        return Err(ApiError::bad_request(&format!(
            "Destination already exists: {}",
            req.dest
        )));
    }

    if abs_src.is_dir() {
        copy_dir_recursive(&abs_src, &abs_dest).map_err(|e| ApiError::internal(&e))?;
    } else {
        // Ensure parent directory exists
        if let Some(parent) = abs_dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ApiError::internal(&format!("Failed to create parent directory: {}", e))
            })?;
        }
        std::fs::copy(&abs_src, &abs_dest)
            .map_err(|e| ApiError::internal(&format!("Failed to copy file: {}", e)))?;
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateFileResponse {
            ok: true,
            path: req.dest.clone(),
        }),
    ))
}

// ─── Content Search API ─────────────────────────────────────────────────

/// Query parameters for workspace content search
#[derive(Debug, Deserialize, Default)]
pub struct SearchQuery {
    /// Regex pattern to search for (case-insensitive by default)
    pub q: Option<String>,
    /// Workspace ID. "__agent_home__" or empty = agent home directory
    pub workspace_id: Option<String>,
    /// Optional comma-separated file glob filter, e.g. "*.rs,*.toml"
    pub include: Option<String>,
    /// Maximum number of match results to return (default 200, max 1000)
    pub max_results: Option<usize>,
    /// Enable case-sensitive matching (default: false = case-insensitive)
    #[serde(default)]
    pub case_sensitive: bool,
    /// Match whole words only — wraps pattern in \b word boundaries
    #[serde(default)]
    pub whole_word: bool,
}

/// A single search match result
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    /// Relative file path within the workspace
    pub file: String,
    /// 1-based line number
    pub line: usize,
    /// 1-based column number (byte offset of match start)
    pub column: usize,
    /// The matching line text (trimmed)
    pub text: String,
}

/// Response for content search
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// Matching results (capped at max_results)
    pub matches: Vec<SearchMatch>,
    /// Total number of matches found (may exceed matches.len() if truncated)
    pub total_matches: usize,
    /// True if results were truncated due to max_results limit
    pub truncated: bool,
}

/// `GET /api/agents/{agent_id}/workspaces/search` — search file contents
///
/// Uses the `ignore` crate (same as ripgrep) for .gitignore-aware file
/// traversal and regex matching. Results are case-insensitive by default.
///
/// Heavy I/O (directory walk + file reading) is offloaded to
/// `tokio::task::spawn_blocking` to avoid blocking the async runtime.
pub async fn search_files(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ApiError>)> {
    let pattern = query.q.as_deref().unwrap_or("");
    if pattern.is_empty() {
        return Err(ApiError::bad_request("Missing required 'q' parameter"));
    }

    // Compile regex with user-controlled case-sensitivity and whole-word options
    let re_pattern = if query.whole_word {
        // Wrap in non-capturing group with word boundaries for whole-word matching
        format!(r"\b(?:{})\b", pattern)
    } else {
        pattern.to_string()
    };
    let re = regex::RegexBuilder::new(&re_pattern)
        .case_insensitive(!query.case_sensitive)
        .build()
        .map_err(|e| ApiError::bad_request(&format!("Invalid regex: {}", e)))?;

    // Resolve workspace root
    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    let max_results = query.max_results.unwrap_or(200).min(1000);
    let include_glob = query.include.map(|s| s.to_string());

    // Offload the heavy I/O work to a blocking thread pool to prevent
    // it from starving the async runtime (tokio).
    let result = tokio::task::spawn_blocking(move || {
        run_search(&workspace_root, &re, include_glob.as_deref(), max_results)
    })
    .await
    .map_err(|_| ApiError::internal("Search task panicked"))?;

    Ok(Json(result))
}

/// Synchronous search logic — runs on a blocking thread pool.
///
/// Walks the workspace tree via `ignore`, reads each text file, and
/// collects regex matches. Binary files and files larger than 1 MiB are
/// skipped to keep search fast.
fn run_search(
    workspace_root: &str,
    re: &regex::Regex,
    include_glob: Option<&str>,
    max_results: usize,
) -> SearchResponse {
    let mut results: Vec<SearchMatch> = Vec::with_capacity(max_results);
    let mut total_matches: usize = 0;
    let mut truncated = false;

    let walker = ignore::WalkBuilder::new(workspace_root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    'outer: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();

        // Skip files larger than 1 MiB — they are unlikely to be
        // human-readable source files and would slow down search.
        if let Ok(meta) = entry.metadata()
            && meta.len() > 1_048_576 {
                continue;
            }

        // Apply file filter if specified (comma-separated globs like "*.rs,*.toml")
        if let Some(glob) = include_glob {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let matched = glob.split(',').any(|g| {
                let pat = g.trim();
                if pat.starts_with("*.") {
                    file_name.ends_with(&pat[1..])
                } else {
                    file_name.contains(pat)
                }
            });
            if !matched {
                continue;
            }
        }

        // Skip known binary file extensions
        if is_binary_path(path) {
            continue;
        }

        // Compute relative path (normalize backslashes to forward slashes)
        let rel_path = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for (line_num, line) in content.lines().enumerate() {
            if let Some(m) = re.find(line) {
                total_matches += 1;
                if results.len() < max_results {
                    results.push(SearchMatch {
                        file: rel_path.clone(),
                        line: line_num + 1,
                        column: m.start() + 1,
                        text: line.trim_end().to_string(),
                    });
                } else {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    SearchResponse {
        matches: results,
        total_matches,
        truncated,
    }
}

/// Check whether a file path has a known binary (non-text) extension.
fn is_binary_path(path: &std::path::Path) -> bool {
    const BINARY_EXTENSIONS: &[&str] = &[
        // Images
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "tiff", "tif",
        // Audio / Video
        "mp3", "mp4", "avi", "mov", "wav", "flac", "ogg", "webm", "mkv", // Archives
        "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", // Object / Library / Binary
        "o", "obj", "a", "so", "dylib", "dll", "exe", "pdb", "lib", "class", "wasm", "bc", "ll",
        // Compiled / Cache
        "pyc", "pyo", "rlib", "rmeta", // Documents
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", // Other binary
        "bin", "dat", "db", "sqlite", "sqlite3", "pack", "idx",
    ];

    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

// ─── Filename Search API ───────────────────────────────────────────────

/// Maximum number of entries to walk before bailing out of filename search.
///
/// Filename search should be O(files) per request. We hard-cap the walk
/// at a generous limit so a malicious or unusually large workspace cannot
/// keep the blocking thread busy forever. The first `MAX_FILENAME_SCAN`
/// eligible entries are scored and the top `limit` are returned.
const MAX_FILENAME_SCAN: usize = 50_000;

/// Default and upper bound for the `limit` query parameter.
const DEFAULT_FILENAME_LIMIT: usize = 50;
const MAX_FILENAME_LIMIT: usize = 200;

/// Query parameters for filename search.
#[derive(Debug, Deserialize, Default)]
pub struct FindQuery {
    /// Search query — matched against file/dir name and relative path.
    /// Case-insensitive, space/segment-aware.
    pub q: Option<String>,
    /// Workspace ID. "__agent_home__" or empty = agent home directory.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Maximum number of results to return (default 50, max 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A single filename search match.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindMatch {
    /// Just the file/dir name (last path segment).
    pub name: String,
    /// Relative path within the workspace, using forward slashes.
    pub rel_path: String,
    /// "file" or "directory".
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Heuristic score (higher = better match). Used by the client to
    /// sort and tie-break; not intended to be stable across versions.
    pub score: u32,
}

/// Response for filename search.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResponse {
    /// Absolute path of the workspace root (normalized, no Windows `\\?\` prefix).
    pub root: String,
    /// Number of entries scanned (capped at `MAX_FILENAME_SCAN`).
    pub scanned: usize,
    /// True when the walk was truncated by `MAX_FILENAME_SCAN` before
    /// the entire workspace was scanned. The client should treat the
    /// results as a partial view.
    pub truncated: bool,
    /// Top results, sorted by score descending, then by path length.
    pub matches: Vec<FindMatch>,
}

/// `GET /api/agents/{agent_id}/workspaces/find` — search for files/dirs by name
///
/// Walks the workspace with the `ignore` crate (respects `.gitignore`,
/// skips hidden/system directories) and ranks entries whose name or path
/// contains every whitespace-/slash-separated segment of the query.
///
/// Matching is purely filename-based — the file content is not read, so
/// the request is fast even on large workspaces. Heavy I/O is offloaded
/// to `tokio::task::spawn_blocking` to keep the async runtime responsive.
///
/// Scoring (higher is better):
/// - 1000 — exact name match (case-insensitive)
/// - 800  — name starts with the full query
/// - 600  — every query segment matches a word boundary in the name
/// - 400  — every query segment is a substring of the name
/// - 200  — every query segment is a substring of the relative path
///
/// Tie-break: shorter path, then alphabetical.
pub async fn find_files(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<FindQuery>,
) -> Result<Json<FindResponse>, (StatusCode, Json<ApiError>)> {
    let pattern = query.q.as_deref().unwrap_or("").trim();
    if pattern.is_empty() {
        return Err(ApiError::bad_request("Missing required 'q' parameter"));
    }
    // Own the pattern so it can be moved into the blocking closure.
    let pattern = pattern.to_string();

    let limit = query
        .limit
        .unwrap_or(DEFAULT_FILENAME_LIMIT)
        .clamp(1, MAX_FILENAME_LIMIT);

    let workspace_root =
        resolve_workspace_root(&state, &agent_id, query.workspace_id.as_deref()).await?;

    // Offload the synchronous walk + scoring to a blocking thread.
    let result = tokio::task::spawn_blocking(move || {
        run_filename_search(&workspace_root, &pattern, limit)
    })
    .await
    .map_err(|_| ApiError::internal("Filename search task panicked"))?;

    Ok(Json(result))
}

/// Synchronous filename search — runs on a blocking thread pool.
///
/// Walks the workspace with `ignore::WalkBuilder` (same defaults as
/// content search), scores each entry against the query, then keeps the
/// top `limit` results in a min-heap-like vec.
fn run_filename_search(workspace_root: &str, pattern: &str, limit: usize) -> FindResponse {
    // Normalize the query into segments. Whitespace and path separators
    // split a query into required-AND segments (matches VS Code-style
    // filename search). Empty segments are dropped.
    let q_lower = pattern.to_lowercase();
    let q_segments: Vec<&str> = q_lower
        .split(|c: char| c.is_whitespace() || c == '/' || c == '\\')
        .filter(|s| !s.is_empty())
        .collect();

    // Strip the Windows extended-length path prefix (\\?\) for the
    // returned `root` field, mirroring `list_tree`.
    let canonical_root = std::path::Path::new(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(workspace_root));
    let canonical_str = canonical_root.to_string_lossy();
    let stripped = canonical_str
        .strip_prefix(r"\\?\")
        .unwrap_or(canonical_str.as_ref());
    let root_str = stripped.replace('\\', "/");

    let walker = ignore::WalkBuilder::new(workspace_root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut scored: Vec<FindMatch> = Vec::new();
    let mut scanned: usize = 0;
    let mut truncated = false;

    'outer: for entry in walker {
        // Skip directory entries themselves — we only return files.
        // (We do follow into directories because the walker handles
        // recursion; we just don't include the directories in results.)
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };

        if !ft.is_file() {
            continue;
        }

        scanned += 1;
        if scanned > MAX_FILENAME_SCAN {
            truncated = true;
            break 'outer;
        }

        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Compute the relative path (always forward-slash, no Windows prefix).
        let rel_path = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let Some(score) = score_match(name, &rel_path, &q_lower, &q_segments) else {
            continue;
        };

        scored.push(FindMatch {
            name: name.to_string(),
            rel_path,
            entry_type: "file".to_string(),
            score,
        });
    }

    // Sort: highest score first, then shorter path, then alphabetical.
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.rel_path.len().cmp(&b.rel_path.len()))
            .then(a.rel_path.cmp(&b.rel_path))
    });

    let total = scored.len();
    let matches: Vec<FindMatch> = scored.into_iter().take(limit).collect();

    FindResponse {
        root: root_str,
        scanned,
        truncated: truncated || total > matches.len(),
        matches,
    }
}

/// Compute a match score for one file. Returns `None` when the file
/// does not match the query at all (so the caller can `continue`).
///
/// The scoring is intentionally simple — fast enough to run inside the
/// blocking walk without buffering or heap allocation per file. All
/// comparisons are case-insensitive (pre-lowered by the caller).
fn score_match(name: &str, rel_path: &str, q_lower: &str, q_segments: &[&str]) -> Option<u32> {
    if q_segments.is_empty() {
        return None;
    }

    let name_lower = name.to_lowercase();

    // Exact name match wins immediately.
    if name_lower == *q_lower {
        return Some(1000);
    }

    // Name starts with the full query (e.g. "Splash" matches "SplashScreen.tsx").
    if name_lower.starts_with(q_lower) {
        return Some(800);
    }

    // Every segment matches a word boundary in the name.
    // Word boundaries: start of string, or after a separator [._ -/].
    if q_segments.iter().all(|seg| match_word_boundary(name, seg)) {
        return Some(600);
    }

    // Every segment is a substring of the name.
    if q_segments.iter().all(|seg| name_lower.contains(seg)) {
        return Some(400);
    }

    // Every segment is a substring of the relative path.
    let path_lower = rel_path.to_lowercase();
    if q_segments.iter().all(|seg| path_lower.contains(seg)) {
        return Some(200);
    }

    None
}

/// Returns true when `seg` (already lowercased) appears in `name` aligned
/// to a word boundary.
///
/// A word boundary is:
/// 1. The start of `name`, or
/// 2. A character class `[._ -/]` immediately preceding the match, or
/// 3. A camelCase transition in the *original* name — a lowercase
///    letter followed by an uppercase letter (e.g. the 'S' in
///    "SplashScreen" follows lowercase 'h' and is uppercase).
///
/// This matches the "camelCase / snake_case / kebab-case" intuition
/// for filename search. We deliberately accept the small extra cost
/// of allocating a lowered copy once per call to keep the logic
/// robust against mixed-case queries.
fn match_word_boundary(name: &str, seg: &str) -> bool {
    if seg.is_empty() {
        return true;
    }
    let name_bytes = name.as_bytes();
    let name_lower = name.to_lowercase();
    let mut start = 0usize;
    while let Some(pos) = name_lower[start..].find(seg) {
        let abs = start + pos;
        let at_word_start =
            abs == 0 || matches!(name_bytes[abs - 1], b'.' | b'_' | b' ' | b'-' | b'/');
        // camelCase boundary — the original char at `abs` is uppercase
        // and the one before it is lowercase.
        let camel_boundary = abs > 0
            && name_bytes[abs - 1].is_ascii_lowercase()
            && name_bytes[abs].is_ascii_uppercase();
        if at_word_start || camel_boundary {
            return true;
        }
        start = abs + 1;
    }
    false
}

// ─── Routes ─────────────────────────────────────────────────────────────

use axum::Router;
use axum::routing::{post, put};

/// Create workspace management routes
pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{agent_id}/workspaces",
            get(list_workspaces).post(add_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/current",
            put(set_current_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/{ws_id}",
            put(update_workspace).delete(delete_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/{ws_id}/prompt-file",
            put(set_prompt_file),
        )
        .route("/api/agents/{agent_id}/workspaces/tree", get(list_tree))
        .route("/api/agents/{agent_id}/workspaces/find", get(find_files))
        .route("/api/agents/{agent_id}/workspaces/file",
            get(read_file)
                .put(write_file)
                .post(create_file)
                .delete(delete_file),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/file-raw",
            get(read_raw_file),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/dir",
            post(create_dir).delete(delete_dir),
        )
        .route("/api/agents/{agent_id}/workspaces/copy", post(copy_item))
        .route(
            "/api/agents/{agent_id}/workspaces/search",
            get(search_files),
        )
        .route(
            "/workspace-files/{agent_id}/{workspace_id}/{*path}",
            get(serve_workspace_ws_file),
        )
        .route(
            "/ws-files/{agent_id}/{*path}",
            get(serve_ws_file),
        )
}

#[cfg(test)]
mod filename_search_tests {
    use super::*;
    use std::fs;

    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Files at various depths to test scoring + walking.
        fs::create_dir_all(root.join("src/components/layout")).unwrap();
        fs::write(root.join("src/components/layout/SplashScreen.tsx"), "").unwrap();
        fs::write(root.join("src/components/layout/StatusBar.tsx"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();
        fs::create_dir_all(root.join("node_modules/react")).unwrap();
        // .gitignore'd via node_modules convention? We use explicit ignores.
        fs::write(root.join("node_modules/react/index.js"), "").unwrap();

        // Add a .gitignore that ignores node_modules so we can verify
        // the walker honors ignore rules.
        fs::write(root.join(".gitignore"), "node_modules\n").unwrap();

        dir
    }

    #[test]
    fn score_exact_name_match() {
        let s = score_match("SplashScreen.tsx", "src/SplashScreen.tsx", "splashscreen.tsx", &["splashscreen.tsx"]).unwrap();
        assert_eq!(s, 1000);
    }

    #[test]
    fn score_prefix_name_match() {
        let s = score_match("SplashScreen.tsx", "src/SplashScreen.tsx", "splash", &["splash"]).unwrap();
        assert_eq!(s, 800);
    }

    #[test]
    fn score_word_boundary_substring() {
        // camelCase boundary
        let s = score_match("SplashScreen.tsx", "src/SplashScreen.tsx", "screen", &["screen"]).unwrap();
        assert_eq!(s, 600);
        // snake_case boundary
        let s2 = score_match("user_profile.ts", "src/user_profile.ts", "profile", &["profile"]).unwrap();
        assert_eq!(s2, 600);
    }

    #[test]
    fn score_substring_only() {
        // "creen" is in the name but not at any word boundary.
        let s = score_match("SplashScreen.tsx", "src/SplashScreen.tsx", "creen", &["creen"]).unwrap();
        assert_eq!(s, 400);
    }

    #[test]
    fn score_path_only() {
        // "layout" is a path segment, not in the filename.
        let s = score_match("SplashScreen.tsx", "src/components/layout/SplashScreen.tsx", "layout/splash", &["layout", "splash"]).unwrap();
        assert_eq!(s, 200);
    }

    #[test]
    fn score_no_match() {
        let s = score_match("SplashScreen.tsx", "src/SplashScreen.tsx", "zzzzz", &["zzzzz"]);
        assert!(s.is_none());
    }

    #[test]
    fn end_to_end_walks_and_ranks() {
        let dir = make_tree();
        let root = dir.path().to_string_lossy();
        let resp = run_filename_search(&root, "SplashScreen", 50);
        assert!(!resp.matches.is_empty(), "expected at least one match");
        // Highest-scored match should be the exact prefix "SplashScreen.tsx"
        assert!(resp.matches[0].rel_path.ends_with("SplashScreen.tsx"));
        // node_modules must be excluded by .gitignore
        assert!(
            !resp
                .matches
                .iter()
                .any(|m| m.rel_path.contains("node_modules")),
            "node_modules should be ignored"
        );
    }

    #[test]
    fn end_to_end_segment_query() {
        let dir = make_tree();
        let root = dir.path().to_string_lossy();
        // Two segments: "layout" + "Splash" — only the deep file has both.
        let resp = run_filename_search(&root, "layout Splash", 50);
        assert!(!resp.matches.is_empty());
        assert!(resp.matches[0].rel_path.ends_with("SplashScreen.tsx"));
    }
}
