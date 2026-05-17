//! Configuration and status HTTP API handlers
//!
//! - GET  /api/config   — get Gateway configuration
//! - PUT  /api/config   — update Gateway configuration
//! - GET  /api/status   — system status (version, running count, memory)
//! - GET  /health       — health check (defined in routes.rs)

use axum::{
    extract::State,
    http::StatusCode,
    Json,
    routing::{delete, get},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the config/status/logs router
pub fn config_routes() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(update_config))
        .route("/api/logs", delete(delete_logs))
}

// ── Response types ────────────────────────────────────────────────────

/// Gateway configuration response
#[derive(Serialize)]
pub struct ConfigResponse {
    pub socket_path: String,
    pub packages_dir: String,
    pub data_dir: String,
    pub log_level: String,
    /// Log file maximum size in MB (0 = disabled, default 10)
    pub log_file_size_mb: u64,
    pub idle_timeout_secs: u64,
    pub dev_mode: bool,
    pub http: HttpConfigResponse,
    /// Default LLM provider (if configured)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Default LLM model (if configured)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Global max output tokens limit (default 32768)
    pub max_output_tokens_limit: u64,
}

/// HTTP config subset
#[derive(Serialize)]
pub struct HttpConfigResponse {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub auth_enabled: bool,
}

/// Config update request
#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    /// Log level (trace/debug/info/warn/error)
    #[serde(default)]
    pub log_level: Option<String>,
    /// Log file maximum size in megabytes (0 = disable split)
    #[serde(default)]
    pub log_file_size_mb: Option<u64>,
    /// Idle timeout in seconds
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    /// Default LLM provider for all agents
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default LLM model for all agents
    #[serde(default)]
    pub default_model: Option<String>,
    /// Global max output tokens limit (caps max_output_tokens in API requests)
    #[serde(default)]
    pub max_output_tokens_limit: Option<u64>,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/config` — get current Gateway configuration
///
/// P0-2 fix: Now returns the actual GatewayConfig from GatewayState
/// instead of hardcoded placeholder values.
pub async fn get_config(
    State(state): State<AppState>,
) -> Result<Json<ConfigResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let config = gw.config.as_ref()
        .ok_or_else(|| ApiError::internal("Gateway config not initialized"))?;

    Ok(Json(ConfigResponse {
        socket_path: config.socket_path.clone(),
        packages_dir: config.packages_dir.clone(),
        data_dir: config.data_dir.clone(),
        log_level: config.log_level.clone(),
        idle_timeout_secs: config.idle_timeout_secs,
        dev_mode: config.dev_mode,
        http: HttpConfigResponse {
            enabled: config.http.enabled,
            host: config.http.host.clone(),
            port: config.http.port,
            auth_enabled: config.http.auth_enabled,
        },
        default_provider: config.default_provider.clone(),
        default_model: config.default_model.clone(),
        max_output_tokens_limit: config.max_output_tokens_limit,
        log_file_size_mb: config.log_file_size_mb,
    }))
}

/// `PUT /api/config` — update Gateway configuration (hot reload)
///
/// P0-2 fix: Now actually applies configuration changes to GatewayState
/// instead of just logging them.
pub async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<UpdateConfigRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut updates = Vec::new();
    if let Some(level) = &body.log_level {
        let valid = ["trace", "debug", "info", "warn", "error"];
        if !valid.contains(&level.as_str()) {
            return Err(ApiError::bad_request(&format!(
                "Invalid log_level '{}'. Must be one of: trace, debug, info, warn, error",
                level
            )));
        }
        updates.push(format!("log_level={}", level));
    }
    if let Some(timeout) = body.idle_timeout_secs {
        updates.push(format!("idle_timeout_secs={}", timeout));
    }
    if let Some(ref provider) = body.default_provider {
        updates.push(format!("default_provider={}", provider));
    }
    if let Some(ref model) = body.default_model {
        updates.push(format!("default_model={}", model));
    }
    if let Some(limit) = body.max_output_tokens_limit {
        updates.push(format!("max_output_tokens_limit={}", limit));
    }
    if let Some(size) = body.log_file_size_mb {
        if size > 1024 {
            return Err(ApiError::bad_request("log_file_size_mb must be between 0 and 1024"));
        }
        updates.push(format!("log_file_size_mb={}", size));
    }

    if updates.is_empty() {
        return Err(ApiError::bad_request("No configuration fields to update"));
    }

    // Apply updates to the stored config
    let mut gw = state.gateway_state.write().await;
    if let Some(config) = &mut gw.config {
        if let Some(level) = &body.log_level {
            config.log_level = level.clone();
        }
        if let Some(timeout) = body.idle_timeout_secs {
            config.idle_timeout_secs = timeout;
        }
        // Update default_provider: Some("name") sets it, Some("") clears it
        if let Some(ref provider) = body.default_provider {
            if provider.is_empty() {
                config.default_provider = None;
            } else {
                config.default_provider = Some(provider.clone());
            }
        }
        // Update default_model: Some("model") sets it, Some("") clears it
        if let Some(ref model) = body.default_model {
            if model.is_empty() {
                config.default_model = None;
            } else {
                config.default_model = Some(model.clone());
            }
        }
        if let Some(limit) = body.max_output_tokens_limit {
            config.max_output_tokens_limit = limit;
        }
        if let Some(size) = body.log_file_size_mb {
            config.log_file_size_mb = size;
        }
    }
    drop(gw);

    // Apply log level change immediately via tracing reload
    if let Some(level) = &body.log_level {
        // 1. Apply to Gateway itself
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = tracing_subscriber::EnvFilter::new(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!("Failed to reload Gateway tracing filter: {}", e);
            } else {
                tracing::info!("Gateway log level changed to {}", level);
            }
        }

        // 2. Push to all connected Runtimes
        if let Some(session_mgr) = &state.session_mgr {
            let mgr = session_mgr.lock().await;
            let agent_ids: Vec<String> = {
                let gw = state.gateway_state.read().await;
                gw.running_agents.keys().cloned().collect()
            };
            for agent_id in agent_ids {
                if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                    let push_result = session.push_message(
                        rollball_core::protocol::GatewayResponse::LogLevelUpdate {
                            log_level: level.clone(),
                        }
                    ).await;
                    if push_result {
                        tracing::info!(agent = %agent_id, "Pushed LogLevelUpdate to Runtime");
                    } else {
                        tracing::warn!(agent = %agent_id, "Failed to push LogLevelUpdate (channel closed)");
                    }
                }
            }
        }
    }

    tracing::info!("Config update applied: {}", updates.join(", "));

    Ok(Json(MessageResponse {
        message: format!("Config updated: {}", updates.join(", ")),
    }))
}

/// `DELETE /api/logs` — delete all log files
///
/// Three-phase cleanup:
/// 1. Push LogRotate to **running** agents via IPC — each Runtime
///    force-rotates to a new log file and deletes old `*.log` files.
/// 2. Delete **Gateway**'s own log files in the project config directory.
/// 3. For **stopped** agents (installed but not running), delete
///    `{install_path}/workspace/logs/*.log` directly via filesystem.
pub async fn delete_logs(
    State(state): State<AppState>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut total_deleted = 0u64;

    // ── Phase 1: Push LogRotate to running agents via IPC ──────────────
    if let Some(session_mgr) = &state.session_mgr {
        let mgr = session_mgr.lock().await;
        let agent_ids: Vec<String> = {
            let gw = state.gateway_state.read().await;
            gw.running_agents.keys().cloned().collect()
        };
        for agent_id in &agent_ids {
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(agent_id) {
                let push_result = session
                    .push_message(rollball_core::protocol::GatewayResponse::LogRotate)
                    .await;
                if push_result {
                    tracing::info!(agent = %agent_id, "Pushed LogRotate to Runtime");
                } else {
                    tracing::warn!(agent = %agent_id, "Failed to push LogRotate (channel closed)");
                }
            }
        }
    }

    // ── Phase 2: Delete Gateway's own log files ───────────────────────
    {
        let log_dir = crate::config::GatewayConfig::project_config_dir().join("logs");
        if log_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&log_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map_or(false, |ext| ext == "log") {
                        if let Err(e) = std::fs::remove_file(&path) {
                            tracing::warn!("Failed to delete Gateway log {:?}: {}", path, e);
                        } else {
                            total_deleted += 1;
                        }
                    }
                }
            }
        }
        tracing::info!("Gateway logs cleaned from {:?}", log_dir);
    }

    // ── Phase 3: Delete stopped agents' workspace logs ────────────────
    {
        let gw = state.gateway_state.read().await;
        let running_ids: std::collections::HashSet<&String> =
            gw.running_agents.keys().collect();
        for (agent_id, info) in &gw.installed_agents {
            if running_ids.contains(agent_id) {
                continue; // running agents handle their own logs via IPC
            }
            let workspace_logs = std::path::PathBuf::from(&info.install_path)
                .join("workspace")
                .join("logs");
            if workspace_logs.exists() {
                if let Ok(entries) = std::fs::read_dir(&workspace_logs) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().map_or(false, |ext| ext == "log") {
                            if let Err(e) = std::fs::remove_file(&path) {
                                tracing::warn!(
                                    agent = %agent_id,
                                    "Failed to delete stopped agent log {:?}: {}",
                                    path, e
                                );
                            } else {
                                total_deleted += 1;
                                tracing::debug!(agent = %agent_id, "Deleted stopped agent log {:?}", path);
                            }
                        }
                    }
                }
            }
        }
    }

    tracing::info!("Log cleanup complete: {} log files deleted", total_deleted);
    Ok(Json(MessageResponse {
        message: format!("Deleted {} log file(s)", total_deleted),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_config_request_deserialization() {
        let json = r#"{"log_level": "debug"}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.log_level, Some("debug".to_string()));
        assert!(req.idle_timeout_secs.is_none());
        assert!(req.default_provider.is_none());
        assert!(req.default_model.is_none());
    }

    #[test]
    fn test_update_config_request_both_fields() {
        let json = r#"{"log_level": "warn", "idle_timeout_secs": 600}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.log_level, Some("warn".to_string()));
        assert_eq!(req.idle_timeout_secs, Some(600));
    }

    #[test]
    fn test_update_config_request_provider_and_model() {
        let json = r#"{"default_provider": "deepseek", "default_model": "deepseek-chat"}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.default_provider, Some("deepseek".to_string()));
        assert_eq!(req.default_model, Some("deepseek-chat".to_string()));
    }

    #[test]
    fn test_config_response_serialization() {
        let resp = ConfigResponse {
            socket_path: "/tmp/gateway.sock".to_string(),
            packages_dir: "/tmp/packages".to_string(),
            data_dir: "/tmp/data".to_string(),
            log_level: "info".to_string(),
            idle_timeout_secs: 300,
            dev_mode: false,
            http: HttpConfigResponse {
                enabled: true,
                host: "127.0.0.1".to_string(),
                port: 19876,
                auth_enabled: false,
            },
            default_provider: Some("deepseek".to_string()),
            default_model: Some("deepseek-chat".to_string()),
            max_output_tokens_limit: 32768,
            log_file_size_mb: 10,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("19876"));
        assert!(json.contains("info"));
        assert!(json.contains("deepseek"));
    }
}
