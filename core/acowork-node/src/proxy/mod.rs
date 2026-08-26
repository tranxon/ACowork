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
//! - **Auth**: §6.8 node-token validation is a Phase 5a concern — the
//!   proxy trusts loopback for now (single-machine Phase 2c).
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

use crate::state::SharedNodeState;

/// Build the node reverse-proxy router.
///
/// `state` is the node process table (read-only here) providing the
/// `{agent_id} → http_port` mapping.
pub fn router(state: SharedNodeState) -> Router {
    Router::new()
        .route("/agents/{id}/{*rest}", any(proxy_agent))
        .with_state(state)
}

/// Forward `/agents/{id}/{*rest}` to `http://127.0.0.1:{http_port}/{rest}`.
///
/// Method, path suffix, querystring, body and headers are forwarded
/// verbatim (minus hop-by-hop headers). The Runtime is the authority
/// for the resource — the proxy does not second-guess status/body.
async fn proxy_agent(
    State(state): State<SharedNodeState>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    // Resolve the agent's loopback HTTP port from the node process table.
    let http_port = {
        let node = state.read().await;
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

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        assert!(is_hop_by_hop_header(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("content-length")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("host")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("content-type")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("authorization")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("x-trace-id")));
    }
}
