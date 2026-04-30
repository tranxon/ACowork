//! Workspace directory management API
//!
//! Manages additional directories that agents can access beyond their workspace.
//! Configuration is stored in `{install_path}/workspace/.agent_workspaces.json`

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use uuid::Uuid;

use crate::http::routes::{ApiError, AppState};

/// Global lock for workspace config file writes to prevent TOCTOU races.
///
/// A single lock is sufficient because concurrent writes to the same
/// agent's config are rare, and config I/O is quick (< 1ms).
static CONFIG_WRITE_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// Workspace directory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDir {
    pub id: String,
    pub path: String,
    pub alias: Option<String>,
    pub access: AccessLevel,
    pub added_at: String,
}

/// Access level for workspace directories
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum AccessLevel {
    ReadOnly,
    ReadWrite,
}

/// Workspace configuration file structure
#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceConfig {
    version: String,
    additional_dirs: Vec<WorkspaceDir>,
}

/// Request to add a workspace directory
#[derive(Debug, Deserialize)]
pub struct AddWorkspaceRequest {
    pub path: String,
    pub alias: Option<String>,
    pub access: AccessLevel,
}

/// Request to update a workspace directory
#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub access: Option<AccessLevel>,
    pub alias: Option<String>,
}

/// List of workspace directories
#[derive(Debug, Serialize)]
pub struct WorkspaceListResponse {
    pub agent_id: String,
    pub workspaces: Vec<WorkspaceDir>,
}

/// `GET /api/agents/{agent_id}/workspaces` — list workspace directories for an agent
pub async fn list_workspaces(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<WorkspaceListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Load workspace config (read-only, no lock needed)
    let config = load_workspace_config(&install_path)
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(WorkspaceListResponse {
        agent_id,
        workspaces: config.additional_dirs,
    }))
}

/// `POST /api/agents/{agent_id}/workspaces` — add a workspace directory
pub async fn add_workspace(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceDir>), (StatusCode, Json<ApiError>)> {
    // Verify agent exists and get install_path
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Validate path exists and is a directory
    if !StdPath::new(&req.path).is_dir() {
        return Err(ApiError::bad_request(&format!("Directory not found: {}", req.path)));
    }

    // Acquire file lock to prevent TOCTOU races
    let _lock = CONFIG_WRITE_LOCK.lock().unwrap();

    // Load existing config
    let mut config = load_workspace_config(&install_path)
        .map_err(|e| ApiError::internal(&e))?;

    // Check for duplicate path
    if config.additional_dirs.iter().any(|d| d.path == req.path) {
        return Err(ApiError::bad_request("Directory already exists in workspace list"));
    }

    // Create new entry (12 hex chars = 48 bit, sufficient collision resistance)
    let new_dir = WorkspaceDir {
        id: format!("ws-{}", &Uuid::new_v4().to_string().replace("-", "")[..12]),
        path: req.path.clone(),
        alias: req.alias,
        access: req.access,
        added_at: chrono::Utc::now().to_rfc3339(),
    };

    // Add to config
    config.additional_dirs.push(new_dir.clone());

    // Save config
    save_workspace_config(&install_path, &config)
        .map_err(|e| ApiError::internal(&e))?;

    Ok((StatusCode::CREATED, Json(new_dir)))
}

/// `PUT /api/agents/{agent_id}/workspaces/{ws_id}` — update a workspace directory
pub async fn update_workspace(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
    Json(req): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceDir>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and get install_path
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Acquire file lock to prevent TOCTOU races
    let _lock = CONFIG_WRITE_LOCK.lock().unwrap();

    // Load config
    let mut config = load_workspace_config(&install_path)
        .map_err(|e| ApiError::internal(&e))?;

    // Find and update directory
    let dir = config.additional_dirs.iter_mut()
        .find(|d| d.id == ws_id)
        .ok_or_else(|| ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)))?;

    if let Some(access) = req.access {
        dir.access = access;
    }
    if let Some(alias) = req.alias {
        dir.alias = Some(alias);
    }

    // Clone before saving (avoid unwrap after consume)
    let updated = dir.clone();

    // Save config
    save_workspace_config(&install_path, &config)
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(updated))
}

/// `DELETE /api/agents/{agent_id}/workspaces/{ws_id}` — remove a workspace directory
pub async fn delete_workspace(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and get install_path
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Acquire file lock to prevent TOCTOU races
    let _lock = CONFIG_WRITE_LOCK.lock().unwrap();

    // Load config
    let mut config = load_workspace_config(&install_path)
        .map_err(|e| ApiError::internal(&e))?;

    // Check if exists
    if !config.additional_dirs.iter().any(|d| d.id == ws_id) {
        return Err(ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)));
    }

    // Remove directory
    config.additional_dirs.retain(|d| d.id != ws_id);

    // Save config
    save_workspace_config(&install_path, &config)
        .map_err(|e| ApiError::internal(&e))?;

    Ok(StatusCode::NO_CONTENT)
}

// ─── Helper Functions ───────────────────────────────────────────────────────

fn workspace_config_path(install_path: &str) -> std::path::PathBuf {
    StdPath::new(install_path)
        .join("workspace")
        .join(".agent_workspaces.json")
}

fn load_workspace_config(install_path: &str) -> Result<WorkspaceConfig, String> {
    let config_path = workspace_config_path(install_path);

    if !config_path.exists() {
        // Return default config
        return Ok(WorkspaceConfig {
            version: "1.0.0".to_string(),
            additional_dirs: Vec::new(),
        });
    }

    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read config: {}", e))?;

    serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse config: {}", e))
}

fn save_workspace_config(
    install_path: &str,
    config: &WorkspaceConfig,
) -> Result<(), String> {
    let config_path = workspace_config_path(install_path);

    // Ensure workspace directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create workspace directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    // Write atomically via temp file + rename to avoid partial writes
    let tmp_path = config_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &content)
        .map_err(|e| format!("Failed to write temp config: {}", e))?;
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("Failed to rename config: {}", e))?;

    Ok(())
}

// ─── Routes ─────────────────────────────────────────────────────────────

use axum::routing::put;
use axum::Router;

/// Create workspace management routes
pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{agent_id}/workspaces",
            get(list_workspaces).post(add_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/{ws_id}",
            put(update_workspace).delete(delete_workspace),
        )
}
