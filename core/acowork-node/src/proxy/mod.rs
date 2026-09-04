//! Node reverse proxy (`:19900`) — ADR-055 §6.4 / §6.17.
//!
//! Routes `/agents/{id}/*` to the machine-local Runtime loopback port
//! (`127.0.0.1:{http_port}/*`) with hop-by-hop header stripping
//! (§6.17), so the Gateway reaches every Runtime on this node through a
//! single port + single auth boundary. Runtime `http_endpoint`
//! registration then points at this proxy instead of the Runtime's
//! loopback address (advertise injection chain §6.3).
//!
//! Two-hop semantics (§6.17):
//! - **Error origin**: a failed upstream fetch returns `502` with
//!   `X-Error-Origin: runtime`; an unknown/not-running agent returns
//!   `503` with `X-Error-Origin: node`. The Gateway adds
//!   `X-Error-Origin: node` when *this* proxy is unreachable, so the
//!   failure layer is always identifiable.
//! - **Error mapping**: Runtime responses pass through verbatim (status
//!   + body); the proxy never fabricates a Runtime business error.
//! - **Streaming**: the proxy is buffered like the Gateway's
//!   `proxy_to_runtime_with_method` (chat streaming goes over MQTT, not
//!   HTTP — ADR-035); SSE/WebSocket streaming lands with the LSP
//!   sidecar in Phase 4.
//! - **Auth** (Phase 5a, §6.8): once the node holds a Gateway-issued
//!   `node_token`, inbound requests MUST carry it in
//!   `X-ACowork-Node-Token` or they get `403` with `X-Error-Origin:
//!   node`. Not-yet-enrolled nodes keep the pre-5a open behavior.
//!
//! **Node keeps the `{agent_id} → loopback port` mapping private**
//! (§6.4): the port comes from [`crate::state::AgentSlot::http_port`],
//! allocated by the Node at spawn time.

use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use reqwest::header::HeaderValue;

use crate::state::NodeHttpState;

/// Build the node reverse-proxy router.
///
/// `state` is the node process table (read-only here) providing the
/// `{agent_id} → http_port` mapping.
pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        // ADR-055 §6.7: node liveness endpoint — the LSP relay's
        // self-exit watchdog (`--gateway-health-url`) probes this, so
        // the relay dies with its parent Node.
        .route("/health", axum::routing::get(health))
        .route("/agents/{id}/{*rest}", any(proxy_agent))
        .with_state(state)
}

/// Liveness probe for the Node itself (ADR-055 §6.7 — the LSP relay
/// watchdog target).
///
/// Returns 200 `{"status":"ok"}` whenever the node HTTP server is
/// serving AND the shared process-table lock is acquirable within
/// 500 ms; returns 503 `{"status":"stalled"}` when a business task
/// holds the write lock across an await (the 2026-08 reaper deadlock
/// class). Probing the lock is what turns "accept loop alive" into
/// "business logic can run": during that incident `/health` stayed
/// 200 (3f688e04 hindsight) while every `/agents/*` request stalled,
/// so neither the relay watchdog nor the node HTTP watchdog could
/// see the hang. With the lock probe, both self-heal triggers fire
/// on a genuine stall.
async fn health(State(state): State<NodeHttpState>) -> Response {
    // 2 s timeout: covers slow-but-valid state-mutating operations
    // (install/uninstall/clone/upgrade/upgrade_publish in control/mod.rs
    // acquire the write lock for the full sync filesystem round-trip
    // and can exceed 500 ms on large packages or slow disks). Anything
    // truly stalled longer than this returns 503 and triggers the
    // relay / node HTTP watchdogs to self-heal.
    match tokio::time::timeout(Duration::from_millis(2000), state.node.read()).await {
        Ok(_guard) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "status": "ok" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "stalled" })),
        )
            .into_response(),
    }
}

/// Forward `/agents/{id}/{*rest}` to `http://127.0.0.1:{http_port}/{rest}`.
///
/// Method, path suffix, querystring, body and headers are forwarded
/// verbatim (minus hop-by-hop headers). The Runtime is the authority
/// for the resource — the proxy does not second-guess status/body.
async fn proxy_agent(
    State(state): State<NodeHttpState>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    // ADR-055 Phase 5a §6.8: a Gateway-issued node_token turns this
    // proxy into an auth boundary — inbound requests MUST present it
    // via `X-ACowork-Node-Token` (constant-time compare). Nodes that
    // have not enrolled yet (token None) keep the open behavior.
    if let Some(expected) = state.identity.read().await.node_token.clone() {
        let provided = headers
            .get("X-ACowork-Node-Token")
            .and_then(|v| v.to_str().ok());
        let ok = provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()));
        if !ok {
            tracing::warn!(
                agent_id = %id,
                "Node proxy rejected request: missing/invalid X-ACowork-Node-Token"
            );
            return (
                StatusCode::FORBIDDEN,
                [(HeaderName::from_static("x-error-origin"), HeaderValue::from_static("node"))],
                axum::Json(serde_json::json!({
                    "error": "invalid node token",
                    "id": id,
                })),
            )
                .into_response();
        }
    }

    // Resolve the agent's loopback HTTP port from the node process table.
    let http_port = {
        let node = state.node.read().await;
        node.agents.get(&id).map(|slot| slot.http_port)
    };

    let Some(http_port) = http_port else {
        // ADR-055 §6.17: Node-known error (Runtime not started) → 503,
        // origin = node. Do NOT fabricate a Runtime business error.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(HeaderName::from_static("x-error-origin"), HeaderValue::from_static("node"))],
            axum::Json(serde_json::json!({
                "error": "agent not running on this node",
                "id": id,
            })),
        )
            .into_response();
    };

    let target_url = if let Some(q) = query {
        format!("http://127.0.0.1:{}/{}?{}", http_port, rest, q)
    } else {
        format!("http://127.0.0.1:{}/{}", http_port, rest)
    };

    tracing::debug!(agent_id = %id, http_port, target_url = %target_url, "Node reverse-proxying to Runtime loopback");

    let client = runtime_http_client();
    let mut request = client.request(method.clone(), &target_url);

    // Forward all non-hop-by-hop headers (RFC 7230 §6.1).
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
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

            let mut builder = Response::builder().status(status);
            *builder.headers_mut().unwrap() = resp_headers;
            builder.body(Body::from(body)).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build proxy response")
                    .into_response()
            })
        }
        Err(e) => {
            tracing::warn!(agent_id = %id, url = %target_url, error = %e, "Failed to proxy to Runtime loopback");
            (
                StatusCode::BAD_GATEWAY,
                [(
                    HeaderName::from_static("x-error-origin"),
                    HeaderValue::from_static("runtime"),
                )],
                axum::Json(serde_json::json!({
                    "error": "failed to connect to Runtime HTTP server",
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Strip hop-by-hop headers (RFC 7230 §6.1) — same list as the
/// Gateway's `proxy.rs` so both hops apply identical semantics.
fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Constant-time byte equality — same discipline as the Gateway's
/// `EnrollmentTokenStore::validate_token`: never leaks prefix/length
/// mismatch timing to a remote attacker probing the header value.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Shared reqwest client with connection pooling (ADR-055 §6.17:
/// keep-alive, no per-request rebuild).
fn runtime_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build node reverse-proxy HTTP client")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::state::{NodeState, NodeHttpState};

    fn http_state(node_token: Option<&str>) -> NodeHttpState {
        NodeHttpState {
            node: Arc::new(RwLock::new(NodeState::new(16))),
            identity: Arc::new(RwLock::new(NodeIdentity {
                node_id: "node-1".to_string(),
                machine_uid: "machine-1".to_string(),
                node_token: node_token.map(str::to_string),
                gateway_addr: None,
                enrollment: EnrollmentState::Enrolled,
                created_at: chrono::Utc::now(),
                enrolled_at: None,
            })),
        }
    }

    fn proxy_request(token: Option<&str>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder()
            .uri("/agents/com.test.foo/status")
            .method("GET");
        if let Some(t) = token {
            builder = builder.header("X-ACowork-Node-Token", t);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        assert!(is_hop_by_hop_header(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("content-length")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("host")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("content-type")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("authorization")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("x-trace-id")));
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[tokio::test]
    async fn enrolled_proxy_rejects_missing_token() {
        let app = router(http_state(Some("secret-token")));
        let resp = app.oneshot(proxy_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("x-error-origin").unwrap(),
            HeaderValue::from_static("node")
        );
    }

    #[tokio::test]
    async fn enrolled_proxy_rejects_wrong_token() {
        let app = router(http_state(Some("secret-token")));
        let resp = app
            .oneshot(proxy_request(Some("wrong-token")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn enrolled_proxy_accepts_matching_token() {
        let app = router(http_state(Some("secret-token")));
        // Token passes the gate; the unknown agent then yields 503,
        // proving the auth check let the request through to routing.
        let resp = app
            .oneshot(proxy_request(Some("secret-token")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unenrolled_proxy_is_open() {
        let app = router(http_state(None));
        // No token required pre-enrollment; unknown agent → 503.
        let resp = app.oneshot(proxy_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn health_request() -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri("/health")
            .method("GET")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn health_answers_200_when_state_lock_is_free() {
        let app = router(http_state(None));
        let resp = app.oneshot(health_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_answers_503_when_state_write_lock_is_held() {
        // Simulate the 2026-08 reaper deadlock: a write guard held
        // across an await stalls every reader, and the health probe
        // must report 503 ("stalled") instead of a healthy 200 so the
        // LSP relay watchdog and the node HTTP watchdog can trigger
        // recovery. The 500 ms probe timeout bounds the test.
        let state = http_state(None);
        // Hold the write lock through the probe (borrow a local Arc so
        // `state` can still move into the router).
        let held = state.node.clone();
        let _guard = held.write().await;
        let app = router(state);
        let resp = app.oneshot(health_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
