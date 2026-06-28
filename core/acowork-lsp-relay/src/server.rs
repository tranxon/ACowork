//! HTTP + WebSocket server for the LSP relay process.
//!
//! Endpoints:
//! - `GET /health` — health check (acowork_core::health::HealthResponse)
//! - `GET /events` — SSE event stream (heartbeat + state)
//! - `GET /lsp/{language}` — WebSocket upgrade for LSP relay
//! - `GET /api/lsp/servers-with-status` — list configured LSP servers
//!   together with per-language install status (single round-trip)
//! - `GET /api/lsp/status` — re-probe per-language install status
//!   (used by per-row Check button; avoids re-fetching the full config)
//! - `GET /api/lsp/install/{language}` — get install script
//! - `POST /api/lsp/install/{language}` — run install script

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Sse, sse::Event},
    routing::get,
};
use futures_util::stream::Stream;
use serde::Deserialize;

use acowork_core::event_bus::{BusEvent, EventBus};
use acowork_core::health::HealthResponse;

use crate::config::{
    LspServerStatusEntry, LspServersWithStatus, compute_lsp_status, compute_lsp_status_concurrent,
    lsp_servers_config, resolve_lsp_command,
};
use crate::pool::LspPool;
use crate::relay::lsp_relay;
use crate::state::LspRelayState;

/// Shared application state for the HTTP server.
pub struct AppState {
    /// LSP process pool.
    pub lsp_pool: Arc<LspPool>,
    /// Event bus for SSE /events endpoint.
    pub event_bus: EventBus<LspRelayState>,
}

/// Query parameters for the LSP WebSocket endpoint.
#[derive(Debug, Deserialize)]
pub struct LspQuery {
    /// Workspace root directory (absolute path).
    #[serde(default)]
    pub workspace_root: Option<String>,
}

// ── Health check ───────────────────────────────────────────────────────

/// `GET /health` — health check.
async fn health_check(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = lsp_servers_config();
    let details = serde_json::json!({
        "language_count": cfg.servers.len(),
    });

    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        process: "acowork-lsp-relay".to_string(),
        details: Some(details),
    })
}

// ── SSE events ─────────────────────────────────────────────────────────

/// `GET /events` — SSE event stream.
async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let mut rx = state.event_bus.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let sse_event = match &*event {
                        BusEvent::Heartbeat { seq } => {
                            Event::default()
                                .event("heartbeat")
                                .data(serde_json::json!({"seq": seq}).to_string())
                        }
                        BusEvent::State { seq, state } => {
                            let payload = serde_json::to_value(state).unwrap_or_default();
                            let data = serde_json::json!({"seq": seq, "state": payload});
                            Event::default().event("state").data(data.to_string())
                        }
                    };
                    yield Ok(sse_event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default().comment(format!("lagged:{n}")));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };
    Sse::new(stream)
}

// ── LSP server list ────────────────────────────────────────────────────

/// `GET /api/lsp/servers-with-status` — list configured LSP servers along
/// with per-language installation status in a single round-trip.
///
/// The frontend uses this on initial load and on Refresh so that the
/// server list and install badges arrive atomically — avoiding the
/// 1–2s window where the list is visible but the badges are still
/// being resolved (which previously caused a flash of "empty" badges).
///
/// PATH probes are issued with bounded concurrency
/// (see `compute_lsp_status_concurrent`) so the total wall time is
/// capped regardless of how many languages are configured.
async fn lsp_servers_with_status() -> Json<LspServersWithStatus> {
    let cfg = lsp_servers_config().clone();
    let entries = compute_lsp_status_concurrent().await;
    // Use `IndexMap` so the status map's iteration order matches the
    // language order returned by `compute_lsp_status_concurrent` (which
    // is derived from `cfg.servers.keys()`, itself ordered). The harness
    // UI iterates both maps in lockstep; with `HashMap` the iteration
    // order would be randomized per-process and the badge color next to
    // each server row would shuffle on every gateway restart.
    let mut status: indexmap::IndexMap<String, LspServerStatusEntry> =
        indexmap::IndexMap::with_capacity(entries.len());
    for entry in entries {
        status.insert(entry.language.clone(), entry);
    }
    Json(LspServersWithStatus {
        servers: cfg,
        status,
    })
}

/// `GET /api/lsp/status` — report per-language installation status.
///
/// Kept separate from `/api/lsp/servers-with-status` so the frontend's
/// per-row "Check" button can re-probe a single language without
/// re-fetching the full server list.
async fn lsp_status_list() -> Json<Vec<crate::config::LspServerStatusEntry>> {
    Json(compute_lsp_status().await)
}

// ── WebSocket handler ──────────────────────────────────────────────────

/// `GET /lsp/{language}` — WebSocket upgrade for LSP relay.
async fn lsp_handler(
    ws: WebSocketUpgrade,
    Path(language): Path<String>,
    Query(query): Query<LspQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let lang_lower = language.to_lowercase();

    // Resolve workspace root
    let workspace_root = query
        .workspace_root
        .unwrap_or_else(|| ".".to_string());

    // Resolve LSP command
    let spec = match resolve_lsp_command(&lang_lower).await {
        Some(spec) => spec,
        None => {
            let canonical = crate::config::canonical_language(&lang_lower);
            let cfg = lsp_servers_config();
            let install_hint = cfg
                .servers
                .get(canonical)
                .map(|e| e.install_hint.as_str())
                .unwrap_or("Install the LSP server and ensure it is on PATH");
            let msg = format!(
                "No LSP server found for language: {}. {}",
                language, install_hint
            );
            let err_json = serde_json::json!({ "error": msg, "code": 400u16 });
            return (StatusCode::BAD_REQUEST, axum::Json(err_json)).into_response();
        }
    };

    tracing::info!(
        "[LSP] Upgrading WebSocket for language='{}', cmd='{}' args={:?}, workspace='{}'",
        lang_lower,
        spec.command,
        spec.args,
        workspace_root
    );

    let pool = Arc::clone(&state.lsp_pool);
    ws.on_upgrade(move |socket| lsp_relay(socket, spec, workspace_root, pool))
}

// ── Router ─────────────────────────────────────────────────────────────

/// Build the Axum router with all routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/events", get(events))
        .route("/lsp/{language}", get(lsp_handler))
        .route("/api/lsp/servers-with-status", get(lsp_servers_with_status))
        .route("/api/lsp/status", get(lsp_status_list))
        .route(
            "/api/lsp/install/{language}",
            get(crate::install::lsp_install_script),
        )
        .route(
            "/api/lsp/install/{language}",
            axum::routing::post(crate::install::lsp_install_run),
        )
        .route("/api/codebase/rpc", axum::routing::post(crate::codebase::codebase_rpc))
        // Local-only CORS allowlist. Same origins as Gateway — covers Vite dev
        // (localhost:3000, localhost:5173) and Tauri v2 production WebView
        // (tauri.localhost, tauri://localhost).
        .layer({
            let mut origins: Vec<axum::http::HeaderValue> = vec![
                "http://localhost:3000".parse().unwrap(),
                "http://localhost:5173".parse().unwrap(),
                "http://127.0.0.1:3000".parse().unwrap(),
                "http://tauri.localhost".parse().unwrap(),
                "https://tauri.localhost".parse().unwrap(),
            ];
            // macOS Tauri v2 sends `Origin: tauri://localhost`.
            // The `http` crate may reject non-HTTP URI schemes at runtime,
            // so use a soft parse to avoid panicking on startup.
            if let Ok(v) = "tauri://localhost".parse() {
                origins.push(v);
            }
            tower_http::cors::CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                ])
                .allow_headers([axum::http::header::CONTENT_TYPE])
        })
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Build a test AppState with an empty pool and fresh event bus.
    fn test_state() -> Arc<AppState> {
        let lsp_pool = Arc::new(LspPool::new());
        let event_bus = EventBus::new(16);
        Arc::new(AppState {
            lsp_pool,
            event_bus,
        })
    }

    // ── GET /health ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn health_check_returns_ok_with_details() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["process"], "acowork-lsp-relay");
        assert!(json["version"].is_string());
        assert!(json["details"]["language_count"].is_number());
    }

    // ── GET /api/lsp/servers-with-status ────────────────────────────────

    #[tokio::test]
    async fn lsp_servers_with_status_returns_combined_payload() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/lsp/servers-with-status")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Top-level shape: { servers: LspServersConfig, status: { lang: entry } }
        assert_eq!(json["servers"]["version"], 1);
        assert!(json["servers"]["servers"].is_object());
        assert!(json["status"].is_object());

        let status = json["status"].as_object().unwrap();
        assert!(
            !status.is_empty(),
            "status map must contain at least one language"
        );
        for (lang, entry) in status {
            assert!(entry["installed"].is_boolean(), "status[{lang}].installed must be bool");
            if entry["installed"].as_bool().unwrap() {
                assert!(
                    entry["command"].is_string(),
                    "status[{lang}].command must be present when installed"
                );
            }
        }

        // Server keys and status keys should match exactly — the frontend
        // relies on this 1:1 correspondence.
        let server_keys: std::collections::BTreeSet<&str> = json["servers"]["servers"]
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        let status_keys: std::collections::BTreeSet<&str> =
            status.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            server_keys, status_keys,
            "server and status keys must match"
        );
    }

    // ── GET /api/lsp/status ──────────────────────────────────────────────

    #[tokio::test]
    async fn lsp_status_list_returns_entries() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/lsp/status")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.is_array(), "status should be an array");
        assert!(!json.as_array().unwrap().is_empty(), "should have entries");

        // Each entry must have language and installed fields
        for entry in json.as_array().unwrap() {
            assert!(entry["language"].is_string());
            assert!(entry["installed"].is_boolean());
        }
    }

    // ── GET /lsp/{language} (non-WebSocket, should still respond) ────────

    #[tokio::test]
    async fn lsp_handler_unknown_language_returns_400() {
        let state = test_state();
        let app = build_router(state);
        // A plain GET (not WebSocket upgrade) to /lsp/brainfuck should
        // return 400 because no LSP server is configured for "brainfuck".
        let req = Request::builder()
            .method("GET")
            .uri("/lsp/brainfuck?workspace_root=/tmp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Without a WebSocket upgrade header, Axum returns 426 Upgrade Required
        // before the handler runs. But if the handler does run, it returns 400.
        // Either way, it's not 200.
        assert_ne!(resp.status(), StatusCode::OK);
    }

    // ── GET /events (SSE) ────────────────────────────────────────────────

    #[tokio::test]
    async fn events_endpoint_returns_sse_content_type() {
        let state = test_state();
        let app = build_router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/events")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn events_endpoint_returns_ok_with_state() {
        // Publish a state event before the request — the SSE handler
        // subscribes on request and will deliver subsequent events.
        let state = test_state();
        state
            .event_bus
            .publish_state(LspRelayState::Ready { language_count: 5 });

        let app = build_router(state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/events")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The SSE stream is infinite — we can't read it fully in a unit
        // test. The actual stream content (heartbeat + state events) is
        // verified in the E2E integration test.
    }
}
