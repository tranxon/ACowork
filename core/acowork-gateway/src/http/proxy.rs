//! Gateway HTTP reverse proxy (ADR-033 Phase 2).
//!
//! For specific large-data query paths, the Gateway does not handle the
//! request itself — instead it reverse-proxies to the Runtime's localhost
//! HTTP server. The Gateway looks up the Runtime's HTTP port from a
//! registry (populated during Runtime registration) and forwards the
//! request. If the Runtime is not registered or has exited, returns 503.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.
//!
//! ```text
//! Desktop ──HTTP──▶ Gateway (:19876) ──HTTP reverse proxy──▶ Runtime localhost HTTP (:random)
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::sync::RwLock;

use crate::http::routes::AppState;

/// Registry mapping agent_id → Runtime HTTP port.
///
/// Populated when Runtime registers via `POST /api/agents/{id}/register`
/// (Phase 2). The Gateway uses this to know where to reverse-proxy
/// large data queries.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHttpRegistry {
    /// agent_id → (http_port, registered_at)
    ports: HashMap<String, u16>,
}

impl RuntimeHttpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Runtime's HTTP port.
    pub fn register(&mut self, agent_id: &str, http_port: u16) {
        tracing::info!(
            agent_id,
            http_port,
            "Runtime HTTP port registered for reverse proxy"
        );
        self.ports.insert(agent_id.to_string(), http_port);
    }

    /// Unregister a Runtime (e.g. on disconnect/stop).
    pub fn unregister(&mut self, agent_id: &str) {
        self.ports.remove(agent_id);
    }

    /// Get the HTTP port for a Runtime.
    pub fn get_port(&self, agent_id: &str) -> Option<u16> {
        self.ports.get(agent_id).copied()
    }

    /// Number of registered Runtimes.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.ports.len()
    }

    /// Whether any Runtimes are registered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.ports.is_empty()
    }
}

/// Thread-safe shared RuntimeHttpRegistry.
pub type SharedRuntimeHttpRegistry = Arc<RwLock<RuntimeHttpRegistry>>;

/// Create a new shared RuntimeHttpRegistry.
pub fn new_shared_registry() -> SharedRuntimeHttpRegistry {
    Arc::new(RwLock::new(RuntimeHttpRegistry::new()))
}

/// Build the reverse proxy router.
///
/// These routes are matched AFTER the regular API routes. If a path
/// matches a proxy pattern, the request is forwarded to the Runtime's
/// localhost HTTP server. Otherwise, the regular handler processes it.
pub fn proxy_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{id}/sessions",
            get(proxy_list_sessions),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/messages",
            get(proxy_get_messages),
        )
        .route(
            "/api/agents/{id}/memory/graph",
            get(proxy_get_memory_graph),
        )
}

/// Reverse-proxy `GET /api/agents/{id}/sessions` to Runtime's `GET /sessions`.
async fn proxy_list_sessions(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &agent_id, "/sessions", "").await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/messages` to Runtime's `GET /sessions/{sid}/messages`.
async fn proxy_get_messages(
    State(state): State<AppState>,
    Path((agent_id, sid)): Path<(String, String)>,
) -> Response {
    let path = format!("/sessions/{}/messages", sid);
    proxy_to_runtime(&state, &agent_id, &path, "").await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/graph` to Runtime's `GET /memory/graph`.
async fn proxy_get_memory_graph(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &agent_id, "/memory/graph", "").await
}

/// Core reverse-proxy logic: look up the Runtime's HTTP port and forward.
async fn proxy_to_runtime(
    state: &AppState,
    agent_id: &str,
    path: &str,
    query: &str,
) -> Response {
    // Look up the Runtime's HTTP port from the registry.
    let registry = match &state.runtime_http_registry {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP proxy registry not initialized",
                    "agent_id": agent_id,
                })),
            )
                .into_response();
        }
    };

    let http_port = {
        let reg = registry.read().await;
        reg.get_port(agent_id)
    };

    let http_port = match http_port {
        Some(port) => port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP port not registered",
                    "agent_id": agent_id,
                    "message": "The Runtime has not registered its HTTP port. Ensure the Runtime is running and has called POST /api/agents/{id}/register."
                })),
            )
                .into_response();
        }
    };

    // Build the target URL
    let target_url = if query.is_empty() {
        format!("http://127.0.0.1:{}{}", http_port, path)
    } else {
        format!("http://127.0.0.1:{}{}?{}", http_port, path, query)
    };

    tracing::debug!(
        agent_id,
        http_port,
        target_url = %target_url,
        "Reverse-proxying to Runtime HTTP server"
    );

    // Forward the request
    let client = runtime_http_client();
    match client.get(&target_url).send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = headers;
            response_builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to Runtime");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to Runtime HTTP server",
                    "agent_id": agent_id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// HTTP client for making proxy requests to Runtime.
///
/// This is a thin wrapper around `reqwest::Client` with appropriate
/// timeouts and configuration for localhost requests.
fn runtime_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("Failed to build Runtime HTTP client")
}

/// Forward a request to the Runtime's localhost HTTP server.
///
/// This is used by the proxy handlers when the Runtime's HTTP port
/// is known. In Phase 2 scaffolding, this is not yet wired to the
/// AppState — it will be activated when the Runtime registration
/// endpoint is implemented.
#[allow(dead_code)]
async fn forward_to_runtime(
    http_port: u16,
    method: reqwest::Method,
    path: &str,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let url = format!("http://127.0.0.1:{}{}", http_port, path);

    let client = runtime_http_client();
    let mut req = client.request(method, &url);

    // Forward relevant headers
    for (key, value) in headers.iter() {
        if key == "host" || key == "content-length" {
            continue;
        }
        req = req.header(key, value);
    }

    match req.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = headers;
            response_builder
                .body(axum::body::Body::from(body))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "Failed to proxy to Runtime");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_http_registry() {
        let mut registry = RuntimeHttpRegistry::new();
        assert!(registry.is_empty());

        registry.register("com.test.agent", 12345);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get_port("com.test.agent"), Some(12345));
        assert_eq!(registry.get_port("com.unknown"), None);

        registry.unregister("com.test.agent");
        assert!(registry.is_empty());
        assert_eq!(registry.get_port("com.test.agent"), None);
    }
}
