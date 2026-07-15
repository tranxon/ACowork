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
use std::time::Duration;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put, delete},
};
use axum::body::Bytes;
use tokio::sync::RwLock;

use crate::http::routes::AppState;

/// Registry mapping id → Runtime HTTP port.
///
/// Populated by [`crate::mqtt::dispatch::handle_plaintext_message`] when the
/// Gateway receives a **retained** `acowork/agents/{id}/http_port` payload
/// from a Runtime (ADR-033). The retained flag is critical: if the Gateway
/// restarts (or starts after the Runtime), the broker replays the last
/// known port so the Gateway can immediately resume reverse-proxying large
/// data queries without waiting for the next Runtime-side publish.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHttpRegistry {
    /// id → (http_port, registered_at)
    ports: HashMap<String, u16>,
}

impl RuntimeHttpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Runtime's HTTP port.
    pub fn register(&mut self, id: &str, http_port: u16) {
        tracing::info!(
            id,
            http_port,
            "Runtime HTTP port registered for reverse proxy"
        );
        self.ports.insert(id.to_string(), http_port);
    }

    /// Unregister a Runtime (e.g. on disconnect/stop).
    pub fn unregister(&mut self, id: &str) {
        self.ports.remove(id);
    }

    /// Get the HTTP port for a Runtime.
    pub fn get_port(&self, id: &str) -> Option<u16> {
        self.ports.get(id).copied()
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
            "/api/agents/{id}/workspaces",
            get(proxy_list_workspaces).post(proxy_add_workspace),
        )
        .route(
            "/api/agents/{id}/workspaces/tree",
            get(proxy_list_tree),
        )
        .route(
            "/api/agents/{id}/sessions",
            get(proxy_list_sessions),
        )
        .route(
            "/api/agents/{id}/latest-session",
            get(proxy_latest_session),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/messages",
            get(proxy_get_messages),
        )
        .route(
            "/api/agents/{id}/memory/nodes",
            get(proxy_memory_nodes),
        )
        .route(
            "/api/agents/{id}/memory/stats",
            get(proxy_memory_stats),
        )
        .route(
            "/api/agents/{id}/memory/nodes/{nid}",
            delete(proxy_memory_delete_node).get(proxy_get_memory_node),
        )
        .route(
            "/api/agents/{id}/memory/consolidate",
            post(proxy_memory_consolidate),
        )
        .route(
            "/api/agents/{id}/memory/graph",
            get(proxy_get_memory_graph),
        )
        // Route 1: Get single session
        .route(
            "/api/agents/{id}/sessions/{sid}",
            get(proxy_get_session),
        )
        // Routes 2-5: Session documents proxy
        .route(
            "/api/agents/{id}/sessions/{sid}/documents",
            post(proxy_upload_document).get(proxy_list_documents),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/documents/{doc_id}",
            get(proxy_read_document).delete(proxy_delete_document),
        )
        // Routes 6-9: Workspace config CRUD
        .route(
            "/api/agents/{id}/workspaces/{ws_id}",
            put(proxy_update_workspace).delete(proxy_delete_workspace),
        )
        .route(
            "/api/agents/{id}/workspaces/{ws_id}/prompt-file",
            put(proxy_set_prompt_file),
        )
        // Routes 11-13: Agent config / tools / status
        .route(
            "/api/agents/{id}/config",
            get(proxy_get_config),
        )
        .route(
            "/api/agents/{id}/tools",
            get(proxy_get_tools),
        )
        .route(
            "/api/agents/{id}/status",
            get(proxy_get_status),
        )
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces` to Runtime's `GET /workspaces`.
async fn proxy_list_workspaces(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &id, "/workspaces", "").await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/tree` to Runtime's `GET /workspaces/tree`.
async fn proxy_list_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/tree", &query).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions` to Runtime's `GET /sessions`.
async fn proxy_list_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/sessions", &query).await
}

/// Reverse-proxy `GET /api/agents/{id}/latest-session` to Runtime's `GET /sessions/latest`.
async fn proxy_latest_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &id, "/sessions/latest", "").await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/messages` to Runtime's `GET /sessions/{sid}/messages`.
async fn proxy_get_messages(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let path = format!("/sessions/{}/messages", sid);
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, &path, &query).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/graph` to Runtime's `GET /memory/graph`.
async fn proxy_get_memory_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &id, "/memory/graph", "").await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/nodes` to Runtime's `GET /memory/nodes`.
async fn proxy_memory_nodes(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/memory/nodes", &query).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/stats` to Runtime's `GET /memory/stats`.
async fn proxy_memory_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    proxy_to_runtime(&state, &id, "/memory/stats", "").await
}

/// Reverse-proxy `DELETE /api/agents/{id}/memory/nodes/{nid}` to Runtime's `DELETE /memory/nodes/{nid}`.
async fn proxy_memory_delete_node(
    State(state): State<AppState>,
    Path((id, nid)): Path<(String, String)>,
) -> Response {
    let path = format!("/memory/nodes/{}", nid);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::DELETE,
        None,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/memory/consolidate` to Runtime's `POST /memory/consolidate`.
///
/// Forwards the inbound request body verbatim so the Desktop's `force` /
/// `retention_days` parameters reach the Runtime. When the client sends
/// no body we forward an empty payload — the Runtime's `trigger_consolidate`
/// handler treats this as "use defaults".
async fn proxy_memory_consolidate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/memory/consolidate",
        "",
        reqwest::Method::POST,
        payload,
    )
    .await
}

// ── New Phase 4 proxy handlers ─────────────────────────────────────────

/// Reverse-proxy `POST /api/agents/{id}/sessions/{sid}/documents`
/// to Runtime's `POST /sessions/{sid}/documents`.
async fn proxy_upload_document(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/documents", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::POST, payload).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/documents`
/// to Runtime's `GET /sessions/{sid}/documents`.
async fn proxy_list_documents(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
) -> Response {
    let path = format!("/sessions/{}/documents", sid);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/documents/{doc_id}`
/// to Runtime's `GET /sessions/{sid}/documents/{doc_id}`.
async fn proxy_read_document(
    State(state): State<AppState>,
    Path((id, sid, doc_id)): Path<(String, String, String)>,
) -> Response {
    let path = format!("/sessions/{}/documents/{}", sid, doc_id);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `DELETE /api/agents/{id}/sessions/{sid}/documents/{doc_id}`
/// to Runtime's `DELETE /sessions/{sid}/documents/{doc_id}`.
async fn proxy_delete_document(
    State(state): State<AppState>,
    Path((id, sid, doc_id)): Path<(String, String, String)>,
) -> Response {
    let path = format!("/sessions/{}/documents/{}", sid, doc_id);
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::DELETE, None).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}`
/// to Runtime's `GET /sessions/{sid}`.
async fn proxy_get_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces`
/// to Runtime's `POST /workspaces`.
async fn proxy_add_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, "/workspaces", "", reqwest::Method::POST, payload).await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `PUT /workspaces/{ws_id}`.
async fn proxy_update_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload).await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}/prompt-file`
/// to Runtime's `PUT /workspaces/{ws_id}/prompt-file`.
async fn proxy_set_prompt_file(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}/prompt-file", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() { None } else { Some(body.to_vec()) };
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::PUT, payload).await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `DELETE /workspaces/{ws_id}`.
async fn proxy_delete_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    proxy_to_runtime_with_method(&state, &id, &path, "", reqwest::Method::DELETE, None).await
}

/// Reverse-proxy `GET /api/agents/{id}/memory/nodes/{nid}`
/// to Runtime's `GET /memory/nodes/{nid}`.
async fn proxy_get_memory_node(
    State(state): State<AppState>,
    Path((id, nid)): Path<(String, String)>,
) -> Response {
    let path = format!("/memory/nodes/{}", nid);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `GET /api/agents/{id}/config`
/// to Runtime's `GET /agents/{id}/config`.
async fn proxy_get_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let path = format!("/agents/{}/config", id);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `GET /api/agents/{id}/tools`
/// to Runtime's `GET /agents/{id}/tools`.
async fn proxy_get_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let path = format!("/agents/{}/tools", id);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Reverse-proxy `GET /api/agents/{id}/status`
/// to Runtime's `GET /agents/{id}/status`.
async fn proxy_get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let path = format!("/agents/{}/status", id);
    proxy_to_runtime(&state, &id, &path, "").await
}

/// Build a query string from a HashMap of params.
fn build_query_string(params: &HashMap<String, String>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect();
    parts.sort(); // deterministic order
    parts.join("&")
}

fn urlencoding(s: &str) -> String {
    // Simple percent-encoding for query params
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

/// Core reverse-proxy logic: look up the Runtime's HTTP port and forward a GET request.
async fn proxy_to_runtime(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
) -> Response {
    proxy_to_runtime_with_method(state, id, path, query, reqwest::Method::GET, None).await
}

/// Core reverse-proxy logic with configurable HTTP method and optional body.
///
/// Supports GET, DELETE, POST etc. for endpoints that aren't pure reads.
/// `body` is forwarded verbatim as the request payload when set
/// (POST/PUT/PATCH). Endpoints with no incoming body should pass `None`.
///
/// Content-Type is intentionally NOT copied from the inbound request — most
/// Runtime memory endpoints currently accept/ignore the body, and reqwest's
/// `body(Vec<u8>)` builder emits the default `application/octet-stream`.
/// Endpoints that require a specific content-type should use
/// [`send_runtime_json`] instead (which builds the body from a typed value).
async fn proxy_to_runtime_with_method(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
    method: reqwest::Method,
    body: Option<Vec<u8>>,
) -> Response {
    // Look up the Runtime's HTTP port from the registry.
    let registry = match &state.runtime_http_registry {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP proxy registry not initialized",
                    "id": id,
                })),
            )
                .into_response();
        }
    };

    let http_port = {
        let reg = registry.read().await;
        reg.get_port(id)
    };

    let http_port = match http_port {
        Some(port) => port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP port not registered",
                    "id": id,
                    "message": "The Gateway has not yet discovered this Runtime's HTTP port. The Runtime should publish a retained message on `acowork/agents/{id}/http_port` at startup (ADR-033). Verify the Runtime is running, has connected to the MQTT broker, and was started with `--http-port 0` so its localhost HTTP server is up."
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
        id,
        http_port,
        target_url = %target_url,
        "Reverse-proxying to Runtime HTTP server"
    );

    // Forward the request
    let client = runtime_http_client();
    let mut request = client.request(method.clone(), &target_url);
    if let Some(ref payload) = body {
        request = request.body(payload.clone());
    }
    match request.send().await {
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
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Fetch JSON from a Runtime HTTP endpoint.
///
/// Looks up the Runtime's HTTP port from the registry, calls `GET {path}`,
/// and returns the parsed JSON body. Used by handlers that need typed
/// responses from Runtime endpoints (e.g. latest-session, session-state).
pub(crate) async fn fetch_runtime_json(
    state: &AppState,
    id: &str,
    path: &str,
) -> Result<serde_json::Value, (StatusCode, axum::Json<crate::http::routes::ApiError>)> {
    send_runtime_json(state, id, path, reqwest::Method::GET, None).await
}

/// Send JSON to a Runtime HTTP endpoint with configurable method and body.
///
/// Looks up the Runtime's HTTP port from the registry, calls `{method} {path}`
/// with optional JSON body, and returns the parsed response.
pub(crate) async fn send_runtime_json(
    state: &AppState,
    id: &str,
    path: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, (StatusCode, axum::Json<crate::http::routes::ApiError>)> {
    use crate::http::routes::ApiError;

    let registry = state.runtime_http_registry.as_ref().ok_or_else(|| {
        ApiError::service_unavailable("Runtime HTTP proxy registry not initialized")
    })?;

    let http_port = {
        let reg = registry.read().await;
        reg.get_port(id)
    };

    let http_port = http_port.ok_or_else(|| {
        ApiError::not_found(&format!(
            "Agent {} is not running (no Runtime HTTP port registered)",
            id
        ))
    })?;

    let url = format!("http://127.0.0.1:{}{}", http_port, path);

    let client = runtime_http_client();
    let mut req = client.request(method, &url);
    if let Some(json_body) = body {
        req = req.json(json_body);
    }
    let resp = req.send().await.map_err(|e| {
        tracing::warn!(error = %e, url = %url, "Failed to fetch from Runtime");
        ApiError::service_unavailable(&format!("Runtime not reachable: {}", e))
    })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        tracing::warn!(error = %e, url = %url, "Failed to parse Runtime JSON response");
        ApiError::internal(&format!("Invalid Runtime response: {}", e))
    })?;

    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        return Err(ApiError::not_found(msg));
    }

    Ok(body)
}

/// HTTP client for making proxy requests to Runtime.
///
/// Uses a static `reqwest::Client` (built once, reused) for connection
/// pooling — reqwest strongly recommends reusing a single client instance.
pub(crate) fn runtime_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build Runtime HTTP client")
    })
}

/// Forward a request to the Runtime's localhost HTTP server.
///
/// This is used by the proxy handlers when the Runtime's HTTP port
/// is known but the proxy layer needs to forward the request
/// out-of-band (e.g. without the AppState registry). The current
/// reverse-proxy path goes through `proxy_to_runtime_with_method`,
/// which uses the retained-port registry populated via MQTT.
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
