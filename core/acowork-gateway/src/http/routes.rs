//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, chat, etc.).

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware::{self, Next},
    routing::get,
};
use axum::extract::Request;
use serde::{Deserialize, Serialize};
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
    /// Tracing reload handle for dynamic log level changes
    pub log_reload_handle: Option<crate::LogReloadHandle>,
    /// Whether CORS is enabled (allows any origin for remote Desktop connections)
    pub cors_enabled: bool,
    /// ADR-033: MQTT Gateway client for publishing control commands to Runtime.
    pub mqtt_gateway_client: Option<Arc<crate::mqtt::GatewayMqttClient>>,
    /// ADR-033: MQTT global resources publisher trigger.
    /// HTTP handlers call `.trigger()` after resource changes to republish.
    pub mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    /// ADR-033: Runtime HTTP port registry for reverse proxy to Runtime localhost HTTP.
    pub runtime_http_registry: Option<crate::http::proxy::SharedRuntimeHttpRegistry>,
    /// ADR-033: Agent registry tracking online/offline status from MQTT.
    pub agent_registry: Option<crate::mqtt::agent_registry::SharedAgentRegistry>,
}

impl AppState {
    /// Create a new AppState with default models cache
    pub fn new(
        gateway_state: SharedHttpState,
        auth: Arc<HttpAuth>,
    ) -> Self {
        Self {
            gateway_state,
            auth,
            log_reload_handle: None,
            cors_enabled: false,
            mqtt_gateway_client: None,
            mqtt_publisher_trigger: None,
            runtime_http_registry: None,
            agent_registry: None,
        }
    }
}

/// Log request Origin and response CORS header for every HTTP request.
/// This middleware runs AFTER the CORS layer, so `Access-Control-Allow-Origin`
/// reflects what the browser actually sees.
async fn log_request_origin(req: Request, next: Next) -> axum::response::Response {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let method = req.method().clone();
    let uri = req.uri().clone();
    let msg = format!("HTTP request: origin={} method={} uri={}", origin, method, uri);
    tracing::info!("{}", msg);
    let response = next.run(req).await;
    let acao = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let msg = format!("HTTP response: status={} access-control-allow-origin={}", response.status(), acao);
    tracing::info!("{}", msg);
    response
}

/// Build the HTTP router with all routes
pub fn build_router(state: AppState) -> Router {
    // When CORS is enabled (remote Desktop ↔ Gateway scenarios),
    // allow any origin. Otherwise, restrict to localhost.
    let cors = if state.cors_enabled {
        tower_http::cors::CorsLayer::permissive().allow_credentials(true)
    } else {
        // Local-only CORS allowlist. Covers two deployment shapes:
        //   1. Vite dev — page served from a Vite dev server on :3000 / :5173
        //   2. Packaged Tauri v2 desktop app:
        //      - Windows / Linux: `https://tauri.localhost`
        //      - macOS:           `tauri://localhost`
        //
        //      Tauri v2 uses HTTPS (not HTTP) for the custom protocol on
        //      Windows/Linux (v2 migration). Without the exact scheme the
        //      browser's `Origin` header won't match and CORS silently
        //      blocks every `fetch()` from the MSI-installed app.
        tower_http::cors::CorsLayer::new()
            .allow_origin({
                let mut origins = vec![
                    "http://localhost:3000".parse().unwrap(),
                    "http://localhost:5173".parse().unwrap(),
                    "http://127.0.0.1:3000".parse().unwrap(),
                    // Tauri v2 production WebView on Windows / Linux
                    // (confirmed via request logging: the WebView sends
                    //  Origin: http://tauri.localhost, not https://)
                    "http://tauri.localhost".parse().unwrap(),
                    "https://tauri.localhost".parse().unwrap(),
                ];
                // macOS Tauri v2 sends `Origin: tauri://localhost`.
                // The `http` crate (1.4.x) may reject non-HTTP URI
                // schemes at runtime. Use a soft parse so the gateway
                // does not panic on startup — macOS users see a CORS
                // error instead of a gateway crash.
                if let Ok(v) = "tauri://localhost".parse() {
                    origins.push(v);
                }
                origins
            })
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
            ])
    };

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        .merge(crate::http::agents::agent_routes())
        .merge(crate::http::chat::chat_routes())
        .merge(crate::http::provider_api::provider_routes())
        .merge(crate::http::config_api::config_routes())
        .merge(crate::http::cron_api::cron_routes())
        .merge(crate::http::models_api::models_routes())
        .merge(crate::http::memory_api::memory_routes())
        .merge(crate::http::skills_api::skills_routes())
        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .merge(crate::http::mcp_catalog_api::mcp_catalog_routes())
        .merge(crate::http::users_api::users_routes())
        .merge(crate::http::embedding_api::embedding_routes())
        .merge(crate::http::fs_browse::fs_routes())
        .merge(crate::http::proxy::proxy_routes())
        .merge(crate::http::debug_mqtt::debug_mqtt_routes())
        .merge(crate::http::settings_api::settings_routes())
        .route("/api/lsp/endpoint", get(lsp_endpoint))
        .with_state(state)
        .layer(middleware::from_fn(log_request_origin))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(cors)
}

// ── Health check ──────────────────────────────────────────────────────

/// Overall health status
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// All checks passed
    Ok,
    /// Some checks failed (system still functional)
    Degraded,
}

/// Individual check result
#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Health check response with dependency checks
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub checks: std::collections::HashMap<String, CheckResult>,
}

/// Minimum disk space for healthy operation (100 MB)
const MIN_DISK_SPACE_BYTES: u64 = 100 * 1024 * 1024;

/// `GET /health` — health check (no auth required)
///
/// Checks critical dependencies and returns an aggregated status:
/// - `"ok"` — all checks passed
/// - `"degraded"` — some checks failed (gRPC unavailable, disk low)
pub async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let mut checks = std::collections::HashMap::new();
    let mut has_degraded = false;

    // 1. IPC check — ADR-033: MQTT-based, always ok
    checks.insert(
        "ipc".to_string(),
        CheckResult {
            status: "ok".to_string(),
            detail: Some("MQTT" .to_string()),
        },
    );

    // 2. CronStore database check
    {
        let gw = state.gateway_state.read().await;
        match &gw.cron_store {
            Some(store) => {
                match store.health_check() {
                    Ok(()) => {
                        checks.insert(
                            "cron_store".to_string(),
                            CheckResult {
                                status: "ok".to_string(),
                                detail: None,
                            },
                        );
                    }
                    Err(e) => {
                        has_degraded = true; // Cron is non-critical
                        checks.insert(
                            "cron_store".to_string(),
                            CheckResult {
                                status: "unhealthy".to_string(),
                                detail: Some(format!("Database error: {}", e)),
                            },
                        );
                    }
                }
            }
            None => {
                has_degraded = true;
                checks.insert(
                    "cron_store".to_string(),
                    CheckResult {
                        status: "degraded".to_string(),
                        detail: Some("CronStore not initialized".to_string()),
                    },
                );
            }
        }
    }

    // 4. Disk space check on data directory
    {
        let gw = state.gateway_state.read().await;
        // Use the data directory for disk space check
        let data_dir = gw
            .config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"));
        match fs2::available_space(&data_dir) {
            Ok(available) => {
                if available < MIN_DISK_SPACE_BYTES {
                    has_degraded = true;
                    checks.insert(
                        "disk".to_string(),
                        CheckResult {
                            status: "degraded".to_string(),
                            detail: Some(format!(
                                "Low disk space: {} MB available",
                                available / (1024 * 1024)
                            )),
                        },
                    );
                } else {
                    checks.insert(
                        "disk".to_string(),
                        CheckResult {
                            status: "ok".to_string(),
                            detail: Some(format!("{} MB available", available / (1024 * 1024))),
                        },
                    );
                }
            }
            Err(e) => {
                has_degraded = true;
                checks.insert(
                    "disk".to_string(),
                    CheckResult {
                        status: "degraded".to_string(),
                        detail: Some(format!("Cannot check disk space: {}", e)),
                    },
                );
            }
        }
    }

    let overall = if has_degraded {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    };

    Json(HealthResponse {
        status: match overall {
            HealthStatus::Ok => "ok".to_string(),
            HealthStatus::Degraded => "degraded".to_string(),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks,
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
pub async fn system_status(State(state): State<AppState>) -> Json<SystemStatusResponse> {
    let gw = state.gateway_state.read().await;

    // ADR-033: Prefer AgentRegistry (MQTT LWT-based) for agent online count.
    // It reflects the broker's protocol-level view via Will Message, which is
    // more accurate than the Gateway's process-level running_agents count.
    let agents_running = if let Some(ref reg) = state.agent_registry {
        reg.read().await.online_count()
    } else {
        gw.running_agents.len()
    };

    Json(SystemStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents_installed: gw.installed_agents.len(),
        agents_running,
        uptime_secs: 0, // TODO: track actual uptime
    })
}

// ── LSP Relay endpoint ────────────────────────────────────────────────

/// Response for `GET /api/lsp/endpoint` — returns the LSP Relay address.
///
/// Desktop App and Agent Runtime use this endpoint to discover the LSP Relay,
/// then connect directly to its WebSocket and JSON-RPC API.
#[derive(Debug, Serialize)]
pub struct LspEndpointResponse {
    pub available: bool,
    pub host: String,
    pub port: Option<u16>,
}

/// `GET /api/lsp/endpoint` — return the LSP Relay's address.
pub async fn lsp_endpoint(State(state): State<AppState>) -> Json<LspEndpointResponse> {
    let gw = state.gateway_state.read().await;
    match &gw.lsp_relay_process {
        Some(eps) if eps.ready => Json(LspEndpointResponse {
            available: true,
            host: "127.0.0.1".to_string(),
            port: Some(eps.port),
        }),
        _ => Json(LspEndpointResponse {
            available: false,
            host: "127.0.0.1".to_string(),
            port: None,
        }),
    }
}

// ── Error response helpers ────────────────────────────────────────────

/// Standard API error response
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
}

impl ApiError {
    pub fn not_found(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::NOT_FOUND,
            Json(Self {
                error: msg.to_string(),
                code: 404,
            }),
        )
    }

    pub fn bad_request(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::BAD_REQUEST,
            Json(Self {
                error: msg.to_string(),
                code: 400,
            }),
        )
    }

    /// ADR-056: unprocessable entity — the request is well-formed but the
    /// referenced entity does not exist (e.g. `default_compact_model`
    /// pointing at an unknown provider_id / model_id). Mirrors the 422
    /// contract in ADR-056 §4.1.
    pub fn unprocessable_entity(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(Self {
                error: msg.to_string(),
                code: 422,
            }),
        )
    }

    pub fn internal(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Self {
                error: msg.to_string(),
                code: 500,
            }),
        )
    }

    pub fn unauthorized(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::UNAUTHORIZED,
            Json(Self {
                error: msg.to_string(),
                code: 401,
            }),
        )
    }

    pub fn service_unavailable(msg: &str) -> (StatusCode, Json<Self>) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Self {
                error: msg.to_string(),
                code: 503,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-http-routes-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
        )
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = test_app_state();
        let resp = health_check(State(state)).await;
        assert_eq!(resp.status, "degraded"); // degraded because no session_mgr/stores
        assert!(!resp.version.is_empty());
        assert!(!resp.checks.is_empty());
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

    // ── LSP endpoint tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_lsp_endpoint_unavailable_when_no_relay() {
        let state = test_app_state();
        let resp = lsp_endpoint(State(state)).await;
        assert!(!resp.available);
        assert_eq!(resp.host, "127.0.0.1");
        assert!(resp.port.is_none());
    }

    #[tokio::test]
    async fn test_lsp_endpoint_available_when_ready() {
        let state = test_app_state();
        {
            let mut gw = state.gateway_state.write().await;
            gw.lsp_relay_process = Some(crate::lifecycle::lsp_relay::LspRelayProcessState {
                pid: 12345,
                port: 19878,
                ready: true,
            });
        }
        let resp = lsp_endpoint(State(state)).await;
        assert!(resp.available);
        assert_eq!(resp.host, "127.0.0.1");
        assert_eq!(resp.port, Some(19878));
    }

    #[tokio::test]
    async fn test_lsp_endpoint_unavailable_when_not_ready() {
        let state = test_app_state();
        {
            let mut gw = state.gateway_state.write().await;
            gw.lsp_relay_process = Some(crate::lifecycle::lsp_relay::LspRelayProcessState {
                pid: 12345,
                port: 19878,
                ready: false,
            });
        }
        let resp = lsp_endpoint(State(state)).await;
        assert!(!resp.available);
        assert!(resp.port.is_none());
    }
}

