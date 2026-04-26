//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, etc.) in later S1.x tasks.

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

/// Shared state for HTTP handlers
pub type SharedHttpState = Arc<RwLock<GatewayState>>;

/// Application state available to all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    /// Shared gateway state
    pub gateway_state: SharedHttpState,
    /// HTTP authentication
    pub auth: Arc<HttpAuth>,
}

/// Build the HTTP router with all routes
pub fn build_router(state: AppState) -> Router {
    let cors = if true {
        // Always add CORS layer; actual CORS policy is controlled by config
        let origin = tower_http::cors::Any;
        tower_http::cors::CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    } else {
        tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
    };

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        .merge(crate::http::agents::agent_routes())
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
