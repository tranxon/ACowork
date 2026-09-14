//! End-to-end tests: ADR-078 — Gateway reverse-proxy of the workspace Git
//! API (`/api/agents/{id}/git/{status,diff,log}`).
//!
//! Spins up a FAKE Runtime HTTP server (records every proxied request's
//! path + query, returns canned ADR-078 JSON) and a real Gateway router
//! (`build_router`) with the fake endpoint registered in the shared
//! `RuntimeHttpRegistry`. Verifies the exact wire contract the Desktop
//! `gitStore` depends on:
//!
//!   1. path + query are forwarded verbatim (workspace_id / path / cached /
//!      limit), percent-encoding preserved for UTF-8 paths;
//!   2. upstream JSON passes through unchanged (Gateway never re-shapes it);
//!   3. unregistered agent → 503 + Retry-After: 2 (with503Retry contract);
//!   4. upstream error status passes through (500 stays 500).
//!
//! Mirrors the `settings_api.rs` in-process router pattern
//! (`tower::ServiceExt::oneshot`), no MQTT broker required.

use std::sync::{Arc, RwLock as StdRwLock};

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::get;
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tower::ServiceExt;

use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::auth::HttpAuth;
use acowork_gateway::http::proxy::{new_shared_registry, SharedRuntimeHttpRegistry};
use acowork_gateway::http::routes::{build_router, AppState};

/// Test-only instance identity (ADR-073: must be a UUIDv4).
const INSTANCE_ID: &str = "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d";

/// Records `(path, raw_query)` of every request the fake Runtime receives.
#[derive(Clone, Default)]
struct Recorder {
    requests: Arc<StdRwLock<Vec<(String, String)>>>,
}

fn record(recorder: &Recorder, req: &Request) {
    let uri = req.uri();
    let query = uri.query().unwrap_or("").to_string();
    recorder
        .requests
        .write()
        .unwrap()
        .push((uri.path().to_string(), query));
}

/// Start a fake Runtime HTTP server on a random loopback port.
/// Returns `(port, recorder)`.
async fn spawn_fake_runtime(fail_status: bool) -> (u16, Recorder) {
    let recorder = Recorder::default();
    let rec = recorder.clone();
    let rec_status = rec.clone();
    let rec_diff = rec.clone();
    let rec_log = rec.clone();
    let app = Router::new()
        .route(
            "/git/status",
            get(move |req: Request| {
                let rec = rec_status.clone();
                async move {
                    record(&rec, &req);
                    if fail_status {
                        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "boom"}))).into_response();
                    }
                    Json(json!({
                        "isRepo": true,
                        "branch": "main",
                        "error": null,
                        "truncated": false,
                        "changes": [
                            { "path": "a.txt", "oldPath": null, "index": "unmodified",
                              "worktree": "modified", "staged": false }
                        ],
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/git/diff",
            get(move |req: Request| {
                let rec = rec_diff.clone();
                async move {
                    record(&rec, &req);
                    Json(json!({
                        "kind": "modified",
                        "original": "hello\n",
                        "modified": "hello world\n",
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/git/log",
            get(move |req: Request| {
                let rec = rec_log.clone();
                async move {
                    record(&rec, &req);
                    Json(json!({
                        "commits": [
                            { "hash": "abc123", "shortHash": "abc123",
                              "author": "e2e", "date": "2025-01-01T00:00:00Z",
                              "subject": "init" }
                        ],
                    }))
                    .into_response()
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake runtime");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake runtime serve");
    });
    (port, recorder)
}

/// Gateway AppState wired to a temp dir + the given registry.
fn gateway_state(registry: SharedRuntimeHttpRegistry) -> (AppState, std::path::PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-git-proxy-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gw_state = GatewayState::new(&dir.to_string_lossy());
    let mut state = AppState::new(
        Arc::new(RwLock::new(gw_state)),  // tokio::sync::RwLock
        Arc::new(HttpAuth::new(false)),
    );
    state.runtime_http_registry = Some(registry);
    (state, dir)
}

/// Send a GET through the Gateway router; returns `(status, body, retry_after)`.
async fn gateway_get(
    router: &axum::Router,
    path_and_query: &str,
) -> (StatusCode, Value, Option<String>) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path_and_query)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap_or("").to_string());
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body, retry_after)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_forwards_query_and_passthrough_json() {
    let (port, recorder) = spawn_fake_runtime(false).await;
    let registry = new_shared_registry();
    registry.write().await.register(INSTANCE_ID, &format!("http://127.0.0.1:{port}"));
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    let (status, body, _) = gateway_get(&router, &format!("/api/agents/{INSTANCE_ID}/git/status?workspace_id=ws-1&x=1")).await;
    assert_eq!(status, StatusCode::OK);
    // Upstream JSON untouched.
    assert_eq!(body["isRepo"], true);
    assert_eq!(body["changes"][0]["path"], "a.txt");
    // The exact query reached the fake Runtime (sorted deterministic order).
    let reqs = recorder.requests.read().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "/git/status");
    assert_eq!(reqs[0].1, "workspace_id=ws-1&x=1", "query forwarded verbatim");
}

#[tokio::test]
async fn diff_forwards_path_and_cached() {
    let (port, recorder) = spawn_fake_runtime(false).await;
    let registry = new_shared_registry();
    registry.write().await.register(INSTANCE_ID, &format!("http://127.0.0.1:{port}"));
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    let (status, body, _) = gateway_get(&router, &format!("/api/agents/{INSTANCE_ID}/git/diff?path=a.txt&cached=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "modified");
    assert_eq!(body["original"], "hello\n");

    let reqs = recorder.requests.read().unwrap().clone();
    assert_eq!(reqs[0].0, "/git/diff");
    assert_eq!(reqs[0].1, "cached=1&path=a.txt", "sorted: cached < path");
}

#[tokio::test]
async fn log_forwards_path_and_limit() {
    let (port, recorder) = spawn_fake_runtime(false).await;
    let registry = new_shared_registry();
    registry.write().await.register(INSTANCE_ID, &format!("http://127.0.0.1:{port}"));
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    let (status, body, _) = gateway_get(&router, &format!("/api/agents/{INSTANCE_ID}/git/log?path=a.txt&limit=50")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["commits"][0]["shortHash"], "abc123");

    let reqs = recorder.requests.read().unwrap().clone();
    assert_eq!(reqs[0].0, "/git/log");
    assert_eq!(reqs[0].1, "limit=50&path=a.txt");
}

#[tokio::test]
async fn utf8_path_is_percent_encoded_when_forwarded() {
    let (port, recorder) = spawn_fake_runtime(false).await;
    let registry = new_shared_registry();
    registry.write().await.register(INSTANCE_ID, &format!("http://127.0.0.1:{port}"));
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    // Desktop builds the query with `URLSearchParams`, so the raw bytes
    // on the wire are already percent-encoded. The Gateway must decode
    // (Query<HashMap>) → re-encode (build_query_string) losslessly, i.e.
    // the Runtime sees the identical percent-encoded value.
    let uri = format!("/api/agents/{INSTANCE_ID}/git/diff?path=%E4%B8%AD%E6%96%87.txt");
    let (status, _body, _) = gateway_get(&router, &uri).await;
    assert_eq!(status, StatusCode::OK);

    let reqs = recorder.requests.read().unwrap().clone();
    assert_eq!(reqs[0].1, "path=%E4%B8%AD%E6%96%87.txt", "UTF-8 path round-trips losslessly");
}

#[tokio::test]
async fn unregistered_agent_returns_503_with_retry_after() {
    let registry = new_shared_registry(); // nothing registered
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    let (status, body, retry_after) =
        gateway_get(&router, &format!("/api/agents/{INSTANCE_ID}/git/status")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry_after.as_deref(), Some("2"), "with503Retry contract");
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn upstream_error_status_passes_through() {
    let (port, _recorder) = spawn_fake_runtime(true).await;
    let registry = new_shared_registry();
    registry.write().await.register(INSTANCE_ID, &format!("http://127.0.0.1:{port}"));
    let (state, _dir) = gateway_state(registry);
    let router = build_router(state);

    let (status, body, _) =
        gateway_get(&router, &format!("/api/agents/{INSTANCE_ID}/git/status")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "500 passes through");
    assert_eq!(body["error"], "boom");
}
