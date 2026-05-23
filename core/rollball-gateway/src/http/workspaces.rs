//! Workspace directory management API
//!
//! Manages additional directories that agents can access beyond their workspace.
//!
//! **ADR-009 (v2)**: Gateway is pure pass-through for workspace config.
//! No persistence to disk. Workspace config is maintained by Agent Runtime
//! (in `.agent_workspaces.json`). Gateway caches the config in `RunningAgentInfo`
//! (in-memory only, cleared on disconnect) to serve HTTP API requests.
//! CRUD operations serialize the full config → push `WorkspaceConfigUpdate` via IPC.
//! Agent must be running (HTTP API returns 409 if not).

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

/// Workspace directory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDir {
    pub id: String,
    pub path: String,
    pub alias: Option<String>,
    pub access: AccessLevel,
    pub added_at: String,
    /// Whether this is the currently selected workspace
    #[serde(default)]
    pub is_current: bool,
    /// Cumulative selection count for context ranking
    #[serde(default)]
    pub select_count: u32,
    /// Last selection timestamp (RFC3339), None if never selected
    #[serde(default)]
    pub last_selected_at: Option<String>,
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

/// Helper: get workspace config from RunningAgentInfo cache.
/// Returns None if agent not running.
async fn get_cached_config(state: &AppState, agent_id: &str) -> Option<WorkspaceConfig> {
    let gw = state.gateway_state.read().await;
    let info = gw.running_agents.get(agent_id)?;
    let json = info.workspace_config_json.as_ref()?;
    serde_json::from_str(json).ok()
}

/// Helper: push WorkspaceConfigUpdate to Runtime and update the cache.
async fn push_and_cache(state: &AppState, agent_id: &str, config: &WorkspaceConfig) -> Result<(), String> {
    let config_json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;

    // Update in-memory cache
    {
        let mut gw = state.gateway_state.write().await;
        if let Some(info) = gw.running_agents.get_mut(agent_id) {
            info.workspace_config_json = Some(config_json.clone());
        } else {
            return Err("Agent not running".to_string());
        }
    }

    // Push to Runtime via IPC
    if let Some(ref session_mgr) = state.session_mgr {
        let session_mgr = session_mgr.clone();
        let agent_id = agent_id.to_string();
        let config_json = config_json.clone();
        tokio::spawn(async move {
            let push_tx = {
                let mgr = session_mgr.lock().await;
                mgr.find_by_agent_id(&agent_id)
                    .and_then(|(_, session)| session.push_sender().cloned())
            };
            if let Some(push_tx) = push_tx {
                let push_msg = rollball_core::protocol::GatewayResponse::WorkspaceConfigUpdate {
                    config_json,
                };
                if push_tx.send(push_msg).await.is_err() {
                    tracing::warn!(
                        "Failed to push WorkspaceConfigUpdate to agent={} (channel closed)",
                        agent_id
                    );
                } else {
                    tracing::info!(
                        "Pushed WorkspaceConfigUpdate to agent={}",
                        agent_id
                    );
                }
            } else {
                tracing::debug!(
                    "Agent {} has no active IPC session, skipping WorkspaceConfigUpdate push",
                    agent_id
                );
            }
        });
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
    let config = get_cached_config(&state, &agent_id).await
        .ok_or_else(|| ApiError::not_found("Agent not running or no workspace config available"))?;

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
    // Validate path exists and is a directory
    if !StdPath::new(&req.path).is_dir() {
        return Err(ApiError::bad_request(&format!("Directory not found: {}", req.path)));
    }

    // Load current config from cache
    let mut config = get_cached_config(&state, &agent_id).await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot add workspace"))?;

    // Check for duplicate path
    if config.additional_dirs.iter().any(|d| d.path == req.path) {
        return Err(ApiError::bad_request("Directory already exists in workspace list"));
    }

    // Create new entry
    let new_dir = WorkspaceDir {
        id: format!("ws-{}", &Uuid::new_v4().to_string().replace("-", "")[..12]),
        path: req.path.clone(),
        alias: req.alias,
        access: req.access,
        added_at: chrono::Utc::now().to_rfc3339(),
        is_current: false,
        select_count: 0,
        last_selected_at: None,
    };

    let result = new_dir.clone();
    config.additional_dirs.push(new_dir);

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config).await
        .map_err(|e| ApiError::internal(&e))?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// `PUT /api/agents/{agent_id}/workspaces/{ws_id}` — update a workspace directory
pub async fn update_workspace(
    State(state): State<AppState>,
    Path((agent_id, ws_id)): Path<(String, String)>,
    Json(req): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceDir>, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id).await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot update workspace"))?;

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

    let updated = dir.clone();

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config).await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(Json(updated))
}

/// `PUT /api/agents/{agent_id}/workspaces/current` — set the current (active) workspace
pub async fn set_current_workspace(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetCurrentWorkspaceRequest>,
) -> Result<Json<WorkspaceListResponse>, (StatusCode, Json<ApiError>)> {
    let mut config = get_cached_config(&state, &agent_id).await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot set workspace"))?;

    // Check that the target workspace exists
    if !config.additional_dirs.iter().any(|d| d.id == req.workspace_id) {
        return Err(ApiError::not_found(&format!(
            "Workspace directory not found: {}",
            req.workspace_id
        )));
    }

    // Clear is_current on all workspaces, then set on target
    let now = chrono::Utc::now().to_rfc3339();
    for dir in &mut config.additional_dirs {
        if dir.id == req.workspace_id {
            dir.is_current = true;
            dir.select_count += 1;
            dir.last_selected_at = Some(now.clone());
        } else {
            dir.is_current = false;
        }
    }

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config).await
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
    let mut config = get_cached_config(&state, &agent_id).await
        .ok_or_else(|| ApiError::not_found("Agent not running — cannot delete workspace"))?;

    // Check if exists
    if !config.additional_dirs.iter().any(|d| d.id == ws_id) {
        return Err(ApiError::not_found(&format!("Workspace directory not found: {}", ws_id)));
    }

    // Remove directory
    config.additional_dirs.retain(|d| d.id != ws_id);

    // Push to Runtime + update cache
    push_and_cache(&state, &agent_id, &config).await
        .map_err(|e| ApiError::internal(&e))?;

    Ok(StatusCode::NO_CONTENT)
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
            "/api/agents/{agent_id}/workspaces/current",
            put(set_current_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/{ws_id}",
            put(update_workspace).delete(delete_workspace),
        )
}
