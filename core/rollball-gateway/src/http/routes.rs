//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, chat, etc.).

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    routing::get,
    Router,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::ipc::session::SessionManager;

/// Shared state for HTTP handlers
pub type SharedHttpState = Arc<RwLock<GatewayState>>;

/// Shared session manager type (same as IPC server)
pub type SharedSessionMgr = Arc<tokio::sync::Mutex<SessionManager>>;

/// Bridge event for forwarding Agent responses to HTTP clients
#[derive(Debug, Clone, serde::Serialize)]
pub struct BridgeEvent {
    /// Agent ID that produced the response
    pub agent_id: String,
    /// Message ID for correlation
    pub message_id: String,
    /// Event type: "chunk", "tool_call", "tool_result", "done"
    pub event_type: String,
    /// Event payload (JSON)
    pub payload: serde_json::Value,
}

/// Application state available to all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    /// Shared gateway state
    pub gateway_state: SharedHttpState,
    /// HTTP authentication
    pub auth: Arc<HttpAuth>,
    /// Shared session manager for pushing messages to agents
    /// Set by Gateway::run() when the IPC server is initialized
    pub session_mgr: Option<SharedSessionMgr>,
    /// Bridge channel for forwarding Agent responses to HTTP clients
    /// The IPC server publishes events; HTTP WebSocket subscribes
    pub bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
}

/// Build the HTTP router with all routes
pub fn build_router(state: AppState) -> Router {
    // P1-1 fix: Restrict CORS to localhost origins only.
    // Allow both localhost and 127.0.0.1 for Desktop App development.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin([
            "http://localhost:3000".parse().unwrap(),
            "http://localhost:5173".parse().unwrap(), // Vite dev server
            "http://127.0.0.1:3000".parse().unwrap(),
        ])
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ]);

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        .merge(crate::http::agents::agent_routes())
        .merge(crate::http::chat::chat_routes())
        .merge(crate::http::vault_api::vault_routes())
        .merge(crate::http::config_api::config_routes())
        .merge(crate::http::permission_api::permission_routes())
        .merge(crate::http::cron_api::cron_routes())
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(cors)
}

// ── Health check ──────────────────────────────────────────────────────

/// Health check response
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// `GET /health` — health check (no auth required)
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// ── System status ─────────────────────────────────────────────────────

/// System status response
#[derive(Serialize)]
pub struct SystemStatusResponse {
    pub version: String,
    pub agents_installed: usize,
    pub agents_running: usize,
    pub uptime_secs: u64,
}

/// `GET /api/status` — system status
pub async fn system_status(
    State(state): State<AppState>,
) -> Json<SystemStatusResponse> {
    let gw = state.gateway_state.read().await;
    Json(SystemStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents_installed: gw.installed_agents.len(),
        agents_running: gw.running_agents.len(),
        uptime_secs: 0, // TODO: track actual uptime
    })
}

// ── Error response helpers ────────────────────────────────────────────

/// Standard API error response
#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
}

impl ApiError {
    pub fn not_found(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::NOT_FOUND, Json(Self {
            error: msg.to_string(),
            code: 404,
        }))
    }

    pub fn bad_request(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::BAD_REQUEST, Json(Self {
            error: msg.to_string(),
            code: 400,
        }))
    }

    pub fn internal(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(Self {
            error: msg.to_string(),
            code: 500,
        }))
    }

    pub fn unauthorized(msg: &str) -> (StatusCode, Json<Self>) {
        (StatusCode::UNAUTHORIZED, Json(Self {
            error: msg.to_string(),
            code: 401,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_state() -> AppState {
        let dir = std::env::temp_dir().join(format!("rollball-test-http-routes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState {
            gateway_state: Arc::new(RwLock::new(gw_state)),
            auth: Arc::new(HttpAuth::new(false)),
            session_mgr: None,
            bridge_tx: None,
        }
    }

    #[tokio::test]
    async fn test_health_check() {
        let resp = health_check().await;
        assert_eq!(resp.status, "ok");
        assert!(!resp.version.is_empty());
    }

    #[tokio::test]
    async fn test_system_status() {
        let state = test_app_state();
        let resp = system_status(State(state)).await;
        assert_eq!(resp.agents_installed, 0);
        assert_eq!(resp.agents_running, 0);
    }

    #[test]
    fn test_build_router() {
        let state = test_app_state();
        let _router = build_router(state);
    }
}
