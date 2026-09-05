//! acowork-doc service HTTP reverse proxy (ADR-064 pattern).
//!
//! The Gateway reverse-proxies `/api/doc/*` to the standalone `acowork-doc`
//! process (`127.0.0.1:{doc_port}/*`), reusing [`crate::http::proxy`]'s
//! transparent proxy mode (ADR-033).
//!
//! ## Path mapping
//!
//! | Gateway public | doc internal |
//! |----------------|--------------|
//! | `/api/doc/health` | `/health` |
//! | `/api/doc/api/tree` | `/api/tree` |
//! | `/api/doc/mcp` | `/mcp` |
//!
//! The doc router keeps its internal `/api/*` prefix; this proxy strips only
//! the `/api/doc` Gateway prefix before forwarding.
//!
//! ## Not-ready semantics
//!
//! When the doc process is not started / restarting (`doc_process` is
//! `None`) the proxy returns **503** with `Retry-After: 2` — the Desktop
//! `with503Retry` backs off per that header (same contract as `/api/pm/*`
//! and `/api/global-resources`).
//!
//! ## Identity (design §9)
//!
//! | Path | Policy |
//! |------|--------|
//! | `/api/doc/*` (REST, Desktop) | **Override** `X-Actor` with trusted `human` (Desktop session user). Client-claimed `X-Actor` (e.g. forged `agent:xxx`) is dropped |
//! | `/api/doc/mcp` (MCP, Agent) | **Validate** `X-MCP-Actor`: agent_id ∈ Gateway `installed_agents` → pass through (trusted); otherwise **strip** (→ anonymous, read-only tools only, design §9.3) |

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::gateway::state::GatewayState;
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;

/// Trusted identity of the Desktop session user (design §9.2:
/// `created_by: "human" | "agent:xxx"`).
const TRUSTED_HUMAN_ACTOR: &str = "human";

/// Build the doc reverse-proxy router.
///
/// Mounted on the Gateway root router (`build_router`), matching `/api/doc/*`.
pub fn doc_proxy_routes() -> Router<AppState> {
    Router::new().route("/api/doc/{*rest}", any(doc_proxy_handler))
}

/// Reverse-proxy `/api/doc/{*rest}` → `http://127.0.0.1:{doc_port}/{rest}`.
///
/// Transparent proxy (RFC 7230 §2.3): forwards method, path, query, body and
/// all non-hop-by-hop headers, injecting trusted identity. Returns 503 when
/// the doc process is not ready.
async fn doc_proxy_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    // Read the doc process actual port + identity-injection state (the
    // supervisor writes `doc_process` once doc is ready).
    let (port, trusted_headers) = {
        let gw = state.gateway_state.read().await;
        let port = gw.doc_process.as_ref().map(|p| p.port);
        let is_mcp = rest == "mcp" || rest.starts_with("mcp/");
        let trusted = build_trusted_headers(&headers, is_mcp, &gw);
        (port, trusted)
    };
    let Some(port) = port else {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "doc service not ready",
                "message": "The doc service process has not started yet (or is restarting). Retry shortly.",
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert("Retry-After", axum::http::HeaderValue::from_static("2"));
        return response;
    };

    let target_url = if let Some(q) = query {
        format!("http://127.0.0.1:{}/{}?{}", port, rest, q)
    } else {
        format!("http://127.0.0.1:{}/{}", port, rest)
    };

    tracing::debug!(port, target_url = %target_url, "Reverse-proxying to doc service");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);

    // Forward the injected trusted headers (RFC 7230 §6.1 strips hop-by-hop).
    for (name, value) in trusted_headers.iter() {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
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
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to doc service");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to doc service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Build the forwarding headers (identity injection).
///
/// - **REST path** (`is_mcp = false`): drop the client-claimed `X-Actor` and
///   inject the trusted `X-Actor: human` — prevents forging `agent:xxx`.
/// - **MCP path** (`is_mcp = true`): validate `X-MCP-Actor` — agent_id in
///   Gateway `installed_agents` passes through; otherwise strip (→ anonymous,
///   read-only tools only).
fn build_trusted_headers(headers: &HeaderMap, is_mcp: bool, gw: &GatewayState) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        if is_mcp {
            // MCP path: only pass through trusted X-MCP-Actor.
            if name == "x-mcp-actor" {
                if let Ok(agent_id) = value.to_str()
                    && gw.is_installed(agent_id)
                {
                    out.insert(name, value.clone());
                }
                // Untrusted / missing → not injected (anonymous read-only,
                // design §9.3).
            } else {
                out.insert(name, value.clone());
            }
        } else {
            // REST path: drop the client-claimed X-Actor (trusted value is
            // injected below).
            if name != "x-actor" {
                out.insert(name, value.clone());
            }
        }
    }
    if !is_mcp {
        out.insert("x-actor", HeaderValue::from_static(TRUSTED_HUMAN_ACTOR));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::{build_router, AppState};
    use crate::lifecycle::doc_supervisor::DocProcessState;

    /// Build a minimal `AppState` (same as `routes.rs::tests::test_app_state`).
    fn test_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-doc-proxy-{}-{}",
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

    /// Start a mock doc server on `127.0.0.1:0` (internal paths keep the
    /// `/api` prefix, as the doc router does). Returns the port.
    async fn start_mock_doc_server() -> u16 {
        let app = axum::Router::new()
            .route("/api/tree", get(|| async { axum::Json(serde_json::json!({ "dirs": [] })) }))
            .route(
                "/api/docs/{id}",
                get(|Path(id): Path<String>| async move {
                    axum::Json(serde_json::json!({ "doc_id": id, "content": "# hi" }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock doc server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock doc server runs");
        });
        port
    }

    /// Simulate the supervisor writing `doc_process` after doc is ready.
    async fn set_doc_process(state: &AppState, port: u16) {
        let mut gw = state.gateway_state.write().await;
        gw.doc_process = Some(DocProcessState {
            pid: 0,
            port,
            ready: true,
        });
    }

    /// `doc_process = None` (not started / restarting): 503 + `Retry-After: 2`.
    #[tokio::test]
    async fn not_ready_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/tree")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "GET /api/doc/api/tree with doc_process=None should return 503"
        );
        assert_eq!(
            response
                .headers()
                .get("Retry-After")
                .map(|v| v.to_str().unwrap()),
            Some("2"),
            "503 must carry Retry-After: 2 for with503Retry"
        );
    }

    /// `doc_process = None`: nested sub-path also 503.
    #[tokio::test]
    async fn not_ready_nested_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/docs/doc-1")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "nested /api/doc/api/docs/* with doc_process=None should return 503"
        );
    }

    /// `doc_process = Some`: `GET /api/doc/api/tree` transparently forwards.
    #[tokio::test]
    async fn forwards_to_doc_process() {
        let state = test_app_state();
        let port = start_mock_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/tree")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "GET /api/doc/api/tree with doc_process=Some should return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert_eq!(
            parsed["dirs"].as_array().unwrap().len(),
            0,
            "mock doc should return an empty dirs list"
        );
    }

    /// `doc_process = Some`: nested path `/api/doc/api/docs/{id}` forwards.
    #[tokio::test]
    async fn forwards_nested_path() {
        let state = test_app_state();
        let port = start_mock_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/docs/doc-42")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "nested /api/doc/api/docs/doc-42 should be forwarded and return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert_eq!(
            parsed["doc_id"],
            serde_json::json!("doc-42"),
            "mock doc should echo the doc id, got: {parsed}"
        );
    }

    /// Mounting the proxy must not break existing Gateway routes (`/health`).
    #[tokio::test]
    async fn preserves_gateway_routes() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "/health should still respond 200 when the doc proxy is mounted"
        );
    }

    // ── Identity injection (design §9) ─────────────────────────────────

    /// Start a mock doc server whose `/echo-headers` and `/mcp` echo headers.
    async fn start_echo_doc_server() -> u16 {
        let echo = |headers: axum::http::HeaderMap| async move {
            let mut map = serde_json::Map::new();
            for (name, value) in headers.iter() {
                map.insert(
                    name.as_str().to_string(),
                    serde_json::json!(value.to_str().unwrap_or_default()),
                );
            }
            axum::Json(serde_json::Value::Object(map))
        };
        let app = axum::Router::new()
            .route("/echo-headers", get(echo))
            .route("/mcp", get(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo doc server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("echo doc server runs");
        });
        port
    }

    /// Inject an installed Agent into GatewayState (`is_installed` check).
    async fn add_installed_agent(state: &AppState, agent_id: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.acowork.architect"
            version = "1.0.0"
            name = "Architect"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
        )
        .expect("manifest parses");
        let mut gw = state.gateway_state.write().await;
        gw.add_installed(crate::gateway::state::AgentInfo {
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Architect".to_string(),
            install_path: "/tmp/architect".to_string(),
            manifest,
            node_id: "local".to_string(),
        });
    }

    /// Proxy a GET to the mock echo server and return the echoed headers.
    async fn proxy_echo_headers(
        state: AppState,
        uri: &str,
        extra_headers: &[(&str, &str)],
    ) -> serde_json::Value {
        let router = build_router(state);
        let mut builder = Request::builder().method("GET").uri(uri);
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let request = builder.body(Body::empty()).expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "proxy to echo server should return 200 for {uri}"
        );
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("echo body should be JSON")
    }

    /// REST path: forged `X-Actor: agent:com.evil` is overridden to `human`.
    #[tokio::test]
    async fn rest_path_overrides_forged_x_actor() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/echo-headers",
            &[("X-Actor", "agent:com.evil")],
        )
        .await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "forged X-Actor must be overridden to trusted human actor, got: {echoed}"
        );
    }

    /// REST path: absent X-Actor → inject `human` (Desktop default operator).
    #[tokio::test]
    async fn rest_path_injects_human_when_absent() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/doc/echo-headers", &[]).await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "absent X-Actor must be injected as human, got: {echoed}"
        );
    }

    /// MCP path: trusted agent (installed) `X-MCP-Actor` passes through.
    #[tokio::test]
    async fn mcp_path_passes_trusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "com.acowork.architect").await;
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-MCP-Actor", "com.acowork.architect")],
        )
        .await;

        assert_eq!(
            echoed["x-mcp-actor"],
            serde_json::json!("com.acowork.architect"),
            "trusted X-MCP-Actor must pass through, got: {echoed}"
        );
    }

    /// MCP path: untrusted agent (not installed) `X-MCP-Actor` is stripped.
    #[tokio::test]
    async fn mcp_path_strips_untrusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "com.acowork.architect").await;
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-MCP-Actor", "com.evil.ghost")],
        )
        .await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "untrusted X-MCP-Actor must be stripped (anonymous), got: {echoed}"
        );
    }

    /// MCP path: absent `X-MCP-Actor` stays anonymous (no injection).
    #[tokio::test]
    async fn mcp_path_keeps_anonymous_when_absent() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/doc/mcp", &[]).await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "absent X-MCP-Actor must stay anonymous, got: {echoed}"
        );
        // MCP path must NOT inject X-Actor (REST-only).
        assert!(
            echoed.get("x-actor").is_none(),
            "MCP path must NOT inject X-Actor, got: {echoed}"
        );
    }
}
