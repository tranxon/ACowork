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
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the config/status router
pub fn config_routes() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(update_config))
}

// ── Response types ────────────────────────────────────────────────────

/// Gateway configuration response
#[derive(Serialize)]
pub struct ConfigResponse {
    pub socket_path: String,
    pub packages_dir: String,
    pub data_dir: String,
    pub log_level: String,
    pub idle_timeout_secs: u64,
    pub dev_mode: bool,
    pub http: HttpConfigResponse,
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
    /// Idle timeout in seconds
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
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
    }
    drop(gw);

    // Apply log level change immediately via tracing reload
    if let Some(level) = &body.log_level {
        // Note: Full tracing reload requires tracing-subscriber reload handle.
        // For now, log the change; the actual tracing level change
        // will be implemented when the tracing reload handle is integrated.
        tracing::info!("Log level change requested: {} (apply via tracing reload handle)", level);
    }

    tracing::info!("Config update applied: {}", updates.join(", "));

    Ok(Json(MessageResponse {
        message: format!("Config updated: {}", updates.join(", ")),
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
    }

    #[test]
    fn test_update_config_request_both_fields() {
        let json = r#"{"log_level": "warn", "idle_timeout_secs": 600}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.log_level, Some("warn".to_string()));
        assert_eq!(req.idle_timeout_secs, Some(600));
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
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("19876"));
        assert!(json.contains("info"));
    }
}
