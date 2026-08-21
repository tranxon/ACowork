//! Static file serving for workspace-backed assets.
//!
//! All write-side workspace operations (file/dir create / write / delete
//! / copy / rename) are handled by the Agent Runtime HTTP server and
//! proxied verbatim through [`crate::http::proxy::proxy_routes`]. This
//! module keeps only what **must** stay on the Gateway side: the
//! static-asset endpoints used by the HTML preview iframe.
//!
//! # Architecture
//!
//! The Runtime is the authoritative owner of workspace config (it writes
//! `<work_dir>/config/agent_workspaces.json`); the Gateway must therefore
//! resolve `workspace_id` against that same on-disk file when serving
//! static assets. [`resolve_workspace_root`] reads
//! `<info.workspace>/config/agent_workspaces.json` directly to look up
//! the absolute path, so the HTML preview iframe renders correctly for
//! both the agent home directory and every additional workspace.
//!
//! The Runtime's JSON-envelope `GET /workspaces/file` is **not** a
//! suitable substitute for these static-asset endpoints — the preview
//! iframe needs raw bytes (HTML / CSS / image binaries), and a base64
//! JSON envelope would break every `<img>`, `<link>`, and `<script>`
//! in the rendered document. That is why this module survives the
//! ADR-040 / ADR-009 v2 "Runtime owns filesystem" refactor — see the
//! module doc on `proxy_routes` for the full list of operations that
//! have moved to the Runtime side.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Workspace configuration file structure (for JSON serialization)
///
/// Mirrors the Runtime's `agent_workspaces.json` schema. The legacy
/// `version` key has been dropped (it carried no logic on either side);
/// serde ignores unknown keys during deserialization, so files written
/// by older versions still parse.
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceConfig {
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

// ─── Static preview path resolution ───────────────────────────────────

/// Resolve the absolute file path for a static-asset request, ensuring
/// it stays within the allowed workspace root. Returns
/// `(root, abs_path, rel_path)`.
///
/// For paths that don't yet exist on disk, canonicalization is skipped
/// and containment is verified by checking for parent-directory
/// traversal (`..`) and absolute-path components. This is the
/// path-traversal guard used by both `/workspace-files/...` and
/// `/ws-files/...`; [`crate::http::proxy::proxy_routes`] handles every
/// other write- and read-side path on the Runtime side.
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

/// Resolve workspace root path for a given agent + workspace_id.
///
/// The Runtime writes additional workspace directories to
/// `<work_dir>/config/agent_workspaces.json`. The Gateway's
/// `RunningAgentInfo::workspace_config_json` field was previously used
/// as an in-memory cache, but the population path (the
/// `UpdateWorkspaceConfig` gRPC message) was never implemented — so
/// reading it always returned `None` for every running agent, breaking
/// every additional-workspace static asset request with a confusing
/// "workspace config not available yet" 404.
///
/// To avoid bolting on a new state-sync coupling just to serve static
/// files, we read the on-disk config that the Runtime maintains
/// itself. The file is written by `RuntimeWorkspaceMutationService`
/// via the `write-tmp-rename` pattern, so reading it here is safe and
/// idempotent. If the file is missing (e.g. the agent has never
/// configured additional workspaces), we treat the unknown id as a
/// 404 — same outcome as a missing entry.
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
        return Ok(info.workspace.clone());
    }

    // `info.workspace` is the agent's work_dir — the same directory the
    // Runtime treats as the canonical root for `config/agent_workspaces.json`.
    let config_path = std::path::Path::new(&info.workspace)
        .join("config")
        .join("agent_workspaces.json");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => {
            // The config file does not exist yet — meaning the agent
            // has no additional workspaces at all. Anything beyond
            // `__agent_home__` is therefore unknown.
            return Err(ApiError::not_found(&format!(
                "Workspace directory not found: {}",
                ws_id
            )));
        }
    };
    let cfg: WorkspaceConfig = serde_json::from_str(&content).map_err(|e| {
        ApiError::internal(&format!(
            "Failed to parse agent_workspaces.json: {}",
            e
        ))
    })?;
    cfg.additional_dirs
        .iter()
        .find(|d| d.id == ws_id)
        .map(|d| d.path.clone())
        .ok_or_else(|| ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)))
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

// ─── Routes ─────────────────────────────────────────────────────────────

/// Static-asset routes for the workspace browser's HTML preview iframe.
///
/// # Architecture note (ADR-009 v2 + ADR-040 + ADR-034 §11.2)
///
/// Per the Runtime-owns-filesystem refactor, every write-side workspace
/// operation is exposed through
/// [`crate::http::proxy::proxy_routes`] which forwards verbatim to the
/// agent's Runtime HTTP server. The only routes this module registers
/// are:
//
///
/// 1. **`/workspace-files/{agent_id}/{workspace_id}/{*path}`** —
///    pass-through for the HTML preview iframe. Streams raw bytes so
///    that `<img>`, `<link>`, `<script>`, and binary sub-resources
///    resolve correctly. Resolves the workspace root via
///    [`resolve_workspace_root`], which now reads
///    `<work_dir>/config/agent_workspaces.json` directly instead of an
///    in-memory cache that was never populated.
/// 2. **`/ws-files/{agent_id}/{*path}`** — legacy alias of the above
///    for callers that don't carry a `workspace_id` segment.
///
/// All file/dir copy / rename / create / write / delete and tree /
/// find / search operations are **not** registered here — they live in
/// `proxy.rs` and run on the Runtime side.
pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/workspace-files/{agent_id}/{workspace_id}/{*path}",
            get(serve_workspace_ws_file),
        )
        .route(
            "/ws-files/{agent_id}/{*path}",
            get(serve_ws_file),
        )
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve absolute paths inside a unique temp directory and return
    /// both the root (as a `String` accepted by `resolve_tree_path`) and
    /// the canonicalised `PathBuf` callers use to seed the filesystem.
    ///
    /// These tests guard the static-asset preview endpoints
    /// (`/workspace-files/...` and `/ws-files/...`). After the
    /// ADR-040 + ADR-034 §11.2 refactor, write-side workspace ops live
    /// on the Runtime side; this module's only filesystem touchpoint
    /// is the static preview, and its path-traversal guard remains
    /// security-critical.
    fn make_temp_workspace(label: &str) -> (String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "acowork-gateway-preview-{}-{}",
            label,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp workspace must be creatable");
        let root = dir.canonicalize().expect("canonicalize temp dir");
        (root.to_string_lossy().to_string(), root)
    }

    #[test]
    fn resolve_tree_path_happy() {
        let (root, root_path) = make_temp_workspace("happy");
        std::fs::write(root_path.join("foo.txt"), "hi").unwrap();

        let (_, abs, rel) = resolve_tree_path(&root, "foo.txt").unwrap();
        assert_eq!(abs, root_path.join("foo.txt"));
        assert_eq!(rel, "foo.txt");
    }

    #[test]
    fn resolve_tree_path_rejects_traversal() {
        let (root, _) = make_temp_workspace("traversal");
        // `..` must be rejected at the validation step (path does not exist).
        let res = resolve_tree_path(&root, "../etc/passwd");
        assert!(res.is_err(), "path traversal must be rejected");
    }

    #[test]
    fn resolve_tree_path_normalizes_absolute_to_relative() {
        let (root, root_path) = make_temp_workspace("absolute-norm");

        // A leading `/` is stripped during normalisation so callers may
        // pass POSIX absolute-looking paths and have them resolved inside
        // the workspace. The actual traversal guard (the `starts_with`
        // check on the canonical path) prevents escape.
        let (_, abs, rel) = resolve_tree_path(&root, "/foo/bar.txt").unwrap();
        assert_eq!(abs, root_path.join("foo/bar.txt"));
        assert_eq!(rel, "foo/bar.txt");
    }

    #[test]
    fn resolve_tree_path_rejects_real_escape_via_canonical_contains() {
        // Even with a leading `/` stripped, the canonicalised candidate
        // must still start with the canonical workspace root. This is the
        // real escape guard for both POSIX (`/etc/...`) and Windows
        // (`C:\\...`) style inputs.
        let (root, _) = make_temp_workspace("escape");
        // Pretend a sibling dir exists at the workspace-root's parent —
        // not strictly `/etc`, but sufficient to exercise the `starts_with`
        // containment branch via a relative `..` escape.
        let res = resolve_tree_path(&root, "subdir/../../escape-attempt");
        // The path does not exist, so it goes through the non-existent
        // branch which validates components. `..` is rejected there.
        assert!(res.is_err(), "`..` escape must be rejected");
    }

    #[test]
    fn resolve_tree_path_allows_nested_new_file() {
        // `subdir/leaf.md` does not exist yet; resolution should still
        // succeed because the static preview legitimately serves any
        // existing file under the workspace root. The check must rely on
        // component analysis, not canonicalization, for non-existent paths.
        let (root, root_path) = make_temp_workspace("nested-new");
        std::fs::create_dir_all(root_path.join("subdir")).unwrap();

        let (_, abs, rel) = resolve_tree_path(&root, "subdir/leaf.md").unwrap();
        assert_eq!(abs, root_path.join("subdir/leaf.md"));
        assert_eq!(rel, "subdir/leaf.md");
    }
}
