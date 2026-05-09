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

/// Workspace configuration file structure
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
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
        is_current: false,
        select_count: 0,
        last_selected_at: None,
    };

    // Add to config
    config.additional_dirs.push(new_dir.clone());

    // Save config
    save_workspace_config(&install_path, &config)
        .map_err(|e| ApiError::internal(&e))?;

    // Push workspace context update to Runtime
    push_workspace_context_update(&state, &agent_id, &install_path);

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

/// `PUT /api/agents/{agent_id}/workspaces/current` — set the current (active) workspace
///
/// Sets the specified workspace as current, increments its select_count,
/// and updates last_selected_at. All other workspaces have is_current cleared.
/// After saving, pushes a WorkspaceContextUpdate to the Agent Runtime via IPC
/// so the agent can update its LLM context in real-time.
pub async fn set_current_workspace(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetCurrentWorkspaceRequest>,
) -> Result<Json<WorkspaceListResponse>, (StatusCode, Json<ApiError>)> {
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

    // Save config atomically
    save_workspace_config(&install_path, &config)
        .map_err(|e| ApiError::internal(&e))?;

    // Push workspace context update to Runtime
    push_workspace_context_update(&state, &agent_id, &install_path);

    Ok(Json(WorkspaceListResponse {
        agent_id,
        workspaces: config.additional_dirs,
    }))
}

/// Push a WorkspaceContextUpdate to the Agent Runtime after any workspace change.
///
/// Best-effort — failure does NOT block the HTTP response. Spawns a tokio task
/// to avoid holding async state across the handler boundary.
fn push_workspace_context_update(
    state: &AppState,
    agent_id: &str,
    install_path: &str,
) {
    if let Some(ref session_mgr) = state.session_mgr {
        let session_mgr = session_mgr.clone();
        let agent_id = agent_id.to_string();
        let install_path = install_path.to_string();
        tokio::spawn(async move {
            let push_tx = {
                let mgr = session_mgr.lock().await;
                mgr.find_by_agent_id(&agent_id)
                    .and_then(|(_, session)| session.push_sender().cloned())
            };
            if let Some(push_tx) = push_tx {
                if let Some((context_text, current_workspace_id, current_workspace_path)) =
                    resolve_workspace_context(&install_path)
                {
                    let push_msg = rollball_core::protocol::GatewayResponse::WorkspaceContextUpdate {
                        context_text,
                        current_workspace_id,
                        current_workspace_path,
                    };
                    if push_tx.send(push_msg).await.is_err() {
                        tracing::warn!(
                            "Failed to push WorkspaceContextUpdate to agent={} (channel closed)",
                            agent_id
                        );
                    } else {
                        tracing::info!(
                            "Pushed WorkspaceContextUpdate to agent={} after workspace change",
                            agent_id
                        );
                    }
                }
            } else {
                tracing::debug!(
                    "Agent {} has no active IPC session, skipping WorkspaceContextUpdate push",
                    agent_id
                );
            }
        });
    }
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

    // Push workspace context update to Runtime
    push_workspace_context_update(&state, &agent_id, &install_path);

    Ok(StatusCode::NO_CONTENT)
}

// ─── Helper Functions ───────────────────────────────────────────────────────

/// Escape special characters for Markdown table cells.
fn escape_markdown_cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace('\n', " ")
        .replace('\r', "")
}

pub fn workspace_config_path(install_path: &str) -> std::path::PathBuf {
    StdPath::new(install_path)
        .join("workspace")
        .join(".agent_workspaces.json")
}

pub fn load_workspace_config(install_path: &str) -> Result<WorkspaceConfig, String> {
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
            "/api/agents/{agent_id}/workspaces/current",
            put(set_current_workspace),
        )
        .route(
            "/api/agents/{agent_id}/workspaces/{ws_id}",
            put(update_workspace).delete(delete_workspace),
        )
}

// ─── Context Computation ─────────────────────────────────────────────────

/// Compute the list of workspaces to inject into LLM context (at most 3).
///
/// Strategy: the currently selected workspace + top 2 by normalized weight.
/// Weight = `normalized_count * 0.3 + recency_score * 0.7` where
/// - `normalized_count` = select_count / max(select_count) across all dirs
/// - `recency_score`   = 1 / (1 + days_since_last_selection)
pub fn compute_context_workspaces(workspaces: &[WorkspaceDir]) -> Vec<&WorkspaceDir> {
    if workspaces.is_empty() {
        return Vec::new();
    }

    // Find the current workspace (if any)
    let current = workspaces.iter().find(|w| w.is_current);

    // Compute max select_count for normalization
    let max_select_count = workspaces
        .iter()
        .map(|w| w.select_count)
        .max()
        .unwrap_or(0);

    // Compute scores for non-current workspaces
    let now = chrono::Utc::now();
    let mut scored: Vec<(f64, &WorkspaceDir)> = workspaces
        .iter()
        .filter(|w| !w.is_current)
        .map(|w| {
            let normalized_count = if max_select_count > 0 {
                w.select_count as f64 / max_select_count as f64
            } else {
                0.0
            };

            let days_since = w
                .last_selected_at
                .as_ref()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| {
                    let duration = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
                    duration.num_days().max(0) as f64
                })
                // Use 1e9 (~2.74M years) instead of f64::MAX to avoid
                // 1.0 + f64::MAX = Infinity which corrupts recency_score.
                .unwrap_or(1e9_f64);

            let recency_score = 1.0 / (1.0 + days_since);
            let score = normalized_count * 0.3 + recency_score * 0.7;
            (score, w)
        })
        .collect();

    // Sort descending by score, take top 2
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top2: Vec<&WorkspaceDir> = scored.into_iter().take(2).map(|(_, w)| w).collect();

    // Build result: current first, then top2 (dedup)
    let mut result = Vec::new();
    if let Some(cur) = current {
        result.push(cur);
    }
    for w in top2 {
        if result.len() >= 3 {
            break;
        }
        result.push(w);
    }

    result
}

/// Format workspace list as LLM-friendly Markdown text.
///
/// Structure:
/// 1. **Current Working Directory** — the user's active workspace (or install dir if none)
/// 2. **Agent Home Directory** — the agent's installation directory (for reference)
/// 3. **Available Workspaces** — user-configured workspace directories (if any)
///
/// `primary_workspace` is the agent's home directory.
/// `workspaces` may be empty (no user-configured workspaces).
pub fn format_workspace_context(workspaces: &[&WorkspaceDir], primary_workspace: &str) -> String {
    let mut buf = String::new();
    buf.push_str("## Workspace Environment\n\n");

    if workspaces.is_empty() {
        // No user workspaces — install dir IS the working directory
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_markdown_cell(primary_workspace)
        ));
        buf.push_str("No additional workspaces have been configured. Use the Agent Home Directory above\n");
        buf.push_str("as the default working directory for all file and shell operations.\n");
        return buf;
    }

    // Find the active workspace (is_current=true)
    let active = workspaces.iter().find(|w| w.is_current);

    // 1. Current Working Directory
    if let Some(current) = active {
        let alias = current.alias.as_deref().unwrap_or("-");
        let access = match current.access {
            AccessLevel::ReadOnly => "read-only",
            AccessLevel::ReadWrite => "read-write",
        };
        buf.push_str(&format!(
            "Current Working Directory: {} ({}, {})\n",
            escape_markdown_cell(&current.path),
            alias,
            access,
        ));
        buf.push_str("This is your currently active workspace.\n\n");
    } else {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_markdown_cell(primary_workspace)
        ));
        buf.push_str("No workspace is currently selected. The agent's home directory is the default working directory.\n\n");
    }

    // 2. Agent Home Directory (reference only)
    buf.push_str(&format!(
        "Agent Home Directory: {} (installation directory)\n\n",
        escape_markdown_cell(primary_workspace)
    ));

    // 3. Available Workspaces (table)
    buf.push_str("### Available Workspaces\n");
    buf.push_str("| # | Alias | Path | Access | Active |\n");
    buf.push_str("|---|-------|------|--------|--------|\n");

    for (i, ws) in workspaces.iter().enumerate() {
        let alias = escape_markdown_cell(ws.alias.as_deref().unwrap_or("-"));
        let access = match ws.access {
            AccessLevel::ReadOnly => "read-only",
            AccessLevel::ReadWrite => "read-write",
        };
        let active_marker = if ws.is_current { "*" } else { "" };
        buf.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            alias,
            escape_markdown_cell(&ws.path),
            access,
            active_marker,
        ));
    }

    buf.push_str("\nIMPORTANT: When performing file operations or running shell commands, ALWAYS use the\n");
    buf.push_str("Current Working Directory path shown above as your starting directory.\n");
    buf.push_str("Do NOT use the Agent Home Directory for project work — it contains the agent's own\n");
    buf.push_str("configuration files, not your project code.\n");
    buf.push_str("All listed directories are authorized for access at the indicated permission level.\n");

    buf
}

/// Resolve workspace context for an agent, suitable for IPC push.
///
/// Reads the agent's workspace config from `{install_path}/workspace/.agent_workspaces.json`,
/// computes which workspaces to inject, formats the context text, and returns
/// the push payload (context_text, current_workspace_id, current_workspace_path).
///
/// Returns `None` if the agent has no workspace config or the config is empty.
pub fn resolve_workspace_context(
    install_path: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let config = load_workspace_config(install_path).ok()?;

    let context_dirs = compute_context_workspaces(&config.additional_dirs);
    let context_text = format_workspace_context(&context_dirs, install_path);

    // Find the current workspace (if any)
    let current = config.additional_dirs.iter().find(|w| w.is_current);
    let current_workspace_id = current.map(|w| w.id.clone());
    let current_workspace_path = current.map(|w| w.path.clone());

    Some((context_text, current_workspace_id, current_workspace_path))
}
