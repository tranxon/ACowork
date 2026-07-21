//! Workspace directory management API
//!
//! Manages additional directories that agents can access beyond their workspace.
//!
//! **ADR-009 (v2)**: Gateway is pure pass-through for workspace config.
//! No persistence to disk. Workspace config is maintained by Agent Runtime
//! (in `agent_workspaces.json`). Gateway caches the config in `RunningAgentInfo`
//! (in-memory only, cleared on disconnect) to serve HTTP API requests.
//! ADR-033: gRPC push replaced with MQTT; workspace mutations are persisted
//! to agent_workspaces.json on startup by Runtime.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Workspace configuration file structure (for JSON serialization)
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceConfig {
    pub version: String,
    pub additional_dirs: Vec<WorkspaceDir>,
}

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
    id: &str,
    workspace_id: Option<&str>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let info = gw
        .running_agents
        .get(id)
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

/// Response type for streaming file responses.
type StreamFileResponse =
    Result<(StatusCode, [(&'static str, String); 2], axum::body::Body), (StatusCode, Json<ApiError>)>;

/// `POST /api/agents/{id}/workspaces/copy` — copy a file or directory
pub async fn copy_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    Json(req): Json<CopyRequest>,
) -> Result<(StatusCode, Json<CreateFileResponse>), (StatusCode, Json<ApiError>)> {
    if req.source.is_empty() || req.dest.is_empty() {
        return Err(ApiError::bad_request(
            "Missing required 'source' or 'dest' parameter",
        ));
    }

    let workspace_root =
        resolve_workspace_root(&state, &id, query.workspace_id.as_deref()).await?;

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
//
// Removed: as of the ADR-040 / ADR-009 v2 refactor, workspace find / search
// run on the Runtime side (see `acowork_runtime::usecases::workspace_query`).
// The Gateway now proxies `GET /api/agents/{id}/workspaces/find` and
// `GET /api/agents/{id}/workspaces/search` verbatim to the Runtime.
// See `crate::http::proxy::proxy_routes` for the proxy handlers.
//

// ─── Routes ─────────────────────────────────────────────────────────────

/// Create workspace management routes
///
/// # Architecture note (ADR-009 v2)
///
/// Per ADR-009 v2 the Gateway is a **pure pass-through** for all
/// filesystem operations — it does not touch disk. The actual file/dir
/// CRUD is therefore exposed through [`crate::http::proxy::proxy_routes`]
/// which forwards verbatim to the agent's Runtime HTTP server. The
/// routes registered here are limited to:
//
///
/// 1. **`/workspaces/copy`** — duplicate a file/dir (server-side, so it
///    works for cross-volume copies and large files).
/// 2. **`/workspace-files/{agent_id}/{workspace_id}/{*path}`** —
///    pass-through for the HTML preview iframe.
/// 3. **`/ws-files/{agent_id}/{*path}`** — alias of the above for
///    legacy callers.
///
/// The previous direct-on-Gateway implementations of
/// `/workspaces/file`, `/workspaces/dir`, `/workspaces/find` and
/// `/workspaces/search` have been removed; those paths now resolve to
/// the proxy in `proxy.rs` (which carries out the read/write/delete /
/// filename-search / content-search on the Runtime side per ADR-009 v2
/// + ADR-040).
pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/workspaces/copy", post(copy_item))
        .route(
            "/workspace-files/{agent_id}/{workspace_id}/{*path}",
            get(serve_workspace_ws_file),
        )
        .route(
            "/ws-files/{agent_id}/{*path}",
            get(serve_ws_file),
        )
}
