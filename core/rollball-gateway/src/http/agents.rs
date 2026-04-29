//! Agent management HTTP API handlers
//!
//! Implements the Agent CRUD and lifecycle endpoints:
//! - GET    /api/agents           — list all agents with status
//! - GET    /api/agents/:id       — get agent detail
//! - POST   /api/agents/install  — install a .agent package
//! - DELETE /api/agents/:id       — uninstall an agent
//! - POST   /api/agents/:id/start — start an agent
//! - POST   /api/agents/:id/stop  — stop a running agent

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the agent management router
pub fn agent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}", get(get_agent_detail).delete(uninstall_agent))
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/{id}/start", post(start_agent))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route("/api/agents/{id}/model", get(get_agent_model))
}

// ── Response types ────────────────────────────────────────────────────

/// Agent list entry
#[derive(Serialize)]
pub struct AgentListResponse {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub running: bool,
}

/// Agent detail response
#[derive(Serialize)]
pub struct AgentDetailResponse {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
}

/// Install request
#[derive(Deserialize)]
pub struct InstallRequest {
    /// Path to the .agent package file
    pub package_path: String,
    /// Skip signature verification (dev mode)
    #[serde(default)]
    pub dev_mode: bool,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// Agent model info response
#[derive(Serialize)]
pub struct AgentModelResponse {
    /// Provider name (e.g. "minimax", "openai")
    pub provider: String,
    /// Currently active model for this agent
    pub model: String,
    /// All available models for this provider
    pub available_models: Vec<String>,
}

// ── Process liveness check ────────────────────────────────────────────

/// Check if a process with the given PID is still alive.
///
/// Uses `/proc/{pid}` on Linux (always available, no I/O cost since procfs
/// is in-memory). On non-Linux platforms, always returns `true` as a fallback.
///
/// Note: There is an inherent TOCTOU race — the process may exit between
/// this check and when the result is used. This is acceptable because the
/// consequence is only a stale `running: true` that self-corrects on the
/// next API call.
#[cfg(target_os = "linux")]
fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(target_os = "linux"))]
fn is_process_alive(_pid: u32) -> bool {
    true // fallback: assume alive if we have a PID record
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents` — list all installed agents
pub async fn list_agents(
    State(state): State<AppState>,
) -> Json<Vec<AgentListResponse>> {
    let gw = state.gateway_state.read().await;
    let agents: Vec<AgentListResponse> = gw
        .installed_agents
        .values()
        .map(|info| {
            // Verify the process is actually alive (not just in running_agents)
            let actually_running = gw.running_agents.get(&info.agent_id)
                .map(|r| is_process_alive(r.pid))
                .unwrap_or(false);
            AgentListResponse {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                version: info.version.clone(),
                running: actually_running,
            }
        })
        .collect();
    Json(agents)
}

/// `GET /api/agents/:id` — get agent detail
pub async fn get_agent_detail(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDetailResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let info = gw.installed_agents.get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let running_info = gw.running_agents.get(&agent_id);
    // Verify the process is actually alive
    let actually_running = running_info.as_ref()
        .map(|r| is_process_alive(r.pid))
        .unwrap_or(false);
    let resp = AgentDetailResponse {
        agent_id: info.agent_id.clone(),
        name: info.name.clone(),
        version: info.version.clone(),
        description: info.manifest.description.clone(),
        author: info.manifest.author.clone(),
        install_path: info.install_path.clone(),
        running: actually_running,
        pid: running_info.map(|r| r.pid),
        started_at: running_info.map(|r| r.started_at.to_rfc3339()),
    };
    Ok(Json(resp))
}

/// `POST /api/agents/install` — install a .agent package
///
/// P1-9 fix: Uses spawn_blocking because install_package performs
/// heavy filesystem operations (ZIP extraction) and synchronous
/// database operations (CronStore insert) that would block the
/// tokio runtime if called directly in an async handler.
pub async fn install_agent(
    State(state): State<AppState>,
    Json(body): Json<InstallRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    // Determine packages dir from Gateway config (canonical source of truth)
    let packages_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
    };

    // Wrap the synchronous install in spawn_blocking
    let package_path_display = body.package_path.clone();
    // Inherit dev_mode from Gateway config if not explicitly set by the client.
    // This ensures that unsigned packages can be installed when the Gateway
    // is running in dev_mode without the client having to know about it.
    let dev_mode = body.dev_mode || {
        let gw = state.gateway_state.read().await;
        gw.config.as_ref().map(|c| c.dev_mode).unwrap_or(false)
    };
    let install_result = tokio::task::spawn_blocking(move || {
        let mut gw = state.gateway_state.blocking_write();
        crate::package_manager::install::install_package(
            std::path::Path::new(&body.package_path),
            &packages_dir,
            &mut gw,
            dev_mode,
        )
    }).await;

    match install_result {
        Ok(Ok(_)) => Ok((StatusCode::CREATED, Json(MessageResponse {
            message: format!("Package installed: {}", package_path_display),
        }))),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Install failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Install task failed: {}", e))),
    }
}

/// `DELETE /api/agents/:id` — uninstall an agent
///
/// P1-9 fix: Uses spawn_blocking because uninstall_package performs
/// synchronous database operations (CronStore delete_by_agent) that
/// would block the tokio runtime if called directly in an async handler.
pub async fn uninstall_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    // Check if agent is running first (lightweight read)
    {
        let gw = state.gateway_state.read().await;
        if gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is running, stop it first", agent_id
            )));
        }
    }

    // Determine packages dir from Gateway config
    let packages_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
    };

    // Wrap the synchronous uninstall in spawn_blocking
    let agent_id_display = agent_id.clone();
    let uninstall_result = tokio::task::spawn_blocking(move || {
        let mut gw = state.gateway_state.blocking_write();
        crate::package_manager::uninstall::uninstall_package(
            &agent_id,
            &packages_dir,
            &mut gw,
        )
    }).await;

    match uninstall_result {
        Ok(Ok(_)) => Ok(Json(MessageResponse {
            message: format!("Agent uninstalled: {}", agent_id_display),
        })),
        Ok(Err(e)) => Err(ApiError::internal(&format!("Uninstall failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Uninstall task failed: {}", e))),
    }
}

/// `POST /api/agents/:id/start` — start an agent
pub async fn start_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_installed(&agent_id) {
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
    }
    if gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!("Agent {} is already running", agent_id)));
    }

    // Use the lifecycle manager to start the agent
    let idle_timeout = 300; // Default idle timeout
    let socket_path = gw.config.as_ref().map(|c| c.socket_path.clone()).unwrap_or_default();
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, socket_path);
    lifecycle.start_agent(&agent_id, &mut gw).await
        .map_err(|e| ApiError::internal(&format!("Start failed: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Agent started: {}", agent_id),
    }))
}

/// `POST /api/agents/:id/stop` — stop a running agent
pub async fn stop_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!("Agent {} is not running", agent_id)));
    }

    let idle_timeout = 300;
    let socket_path = gw.config.as_ref().map(|c| c.socket_path.clone()).unwrap_or_default();
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, socket_path);
    lifecycle.stop_agent(&agent_id, &mut gw).await
        .map_err(|e| ApiError::internal(&format!("Stop failed: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Agent stopped: {}", agent_id),
    }))
}

/// `GET /api/agents/:id/model` — get the current active model for an agent
///
/// Reads the per-agent model preference from the workspace `.agent_model.json` file.
/// If no per-agent preference exists, falls back to the Gateway config default_model,
/// then the Vault entry's default_model (models[0]).
pub async fn get_agent_model(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentModelResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    let info = gw.installed_agents.get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    // Resolve provider and models from Gateway config / Vault
    let default_provider = gw.config.as_ref()
        .and_then(|c| c.default_provider.as_deref())
        .map(|s| s.to_string())
        .or_else(|| gw.vault.list_providers().first().cloned());

    let provider_name = match default_provider {
        Some(name) => name,
        None => return Err(ApiError::not_found("No provider configured in Vault")),
    };

    let vault_entry = gw.vault.get_provider(&provider_name)
        .map_err(|e| ApiError::internal(&format!("Vault error: {}", e)))?;

    let config_default_model = gw.config.as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Gateway-level default model (config > Vault default_model)
    let gateway_model = config_default_model
        .map(|m| m.to_string())
        .or(vault_entry.default_model.clone())
        .unwrap_or_default();

    // Try reading per-agent model preference from workspace
    let workspace = std::path::Path::new(&info.install_path).join("workspace");
    let model_path = workspace.join(".agent_model.json");
    let active_model = if model_path.exists() {
        match std::fs::read_to_string(&model_path) {
            Ok(content) => {
                // Parse only the "model" field
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&content) {
                    obj.get("model")
                        .and_then(|v| v.as_str())
                        .map(|m| m.to_string())
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };

    // Use per-agent preference if available and valid, otherwise gateway default
    let resolved_model = match active_model {
        Some(ref m) if vault_entry.models.contains(m) => m.clone(),
        _ => gateway_model,
    };

    Ok(Json(AgentModelResponse {
        provider: provider_name,
        model: resolved_model,
        available_models: vault_entry.models,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_list_response_serialization() {
        let resp = AgentListResponse {
            agent_id: "com.example.weather".to_string(),
            name: "Weather Agent".to_string(),
            version: "1.0.0".to_string(),
            running: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("com.example.weather"));
        assert!(json.contains("Weather Agent"));
    }

    #[test]
    fn test_install_request_deserialization() {
        let json = r#"{"package_path": "/tmp/weather.agent"}"#;
        let req: InstallRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.package_path, "/tmp/weather.agent");
    }

    #[test]
    fn test_message_response_serialization() {
        let resp = MessageResponse {
            message: "Agent started".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Agent started"));
    }
}
