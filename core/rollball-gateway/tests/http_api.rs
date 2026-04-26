//! HTTP API integration tests
//!
//! Tests all HTTP API endpoints using Axum's test framework (tower::ServiceExt).

use axum::body::Body;
use axum::http::{Request, Method, StatusCode};
use tower::ServiceExt; // for oneshot()

use rollball_gateway::http::routes::{AppState, build_router};
use rollball_gateway::http::auth::HttpAuth;
use rollball_gateway::gateway::state::GatewayState;

fn create_test_app() -> axum::Router {
    let dir = std::env::temp_dir().join(format!(
        "rollball-test-http-api-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let gw_state = GatewayState::new(&dir.to_string_lossy());
    let state = AppState {
        gateway_state: std::sync::Arc::new(tokio::sync::RwLock::new(gw_state)),
        auth: std::sync::Arc::new(HttpAuth::new(false)),
        session_mgr: None,
        bridge_tx: None,
    };
    build_router(state)
}

// ── Health check ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_health_check() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(!json["version"].as_str().unwrap().is_empty());
}

// ── System status ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_system_status() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agents_installed"], 0);
    assert_eq!(json["agents_running"], 0);
}

// ── Agent list (empty) ────────────────────────────────────────────────

#[tokio::test]
async fn test_list_agents_empty() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

// ── Agent detail (not found) ──────────────────────────────────────────

#[tokio::test]
async fn test_get_agent_not_found() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/agents/com.example.nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Send message to non-existent agent ────────────────────────────────

#[tokio::test]
async fn test_send_message_agent_not_found() {
    let app = create_test_app();

    let body = serde_json::json!({
        "content": "Hello"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/agents/com.example.nonexistent/message")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Send message to non-running agent ─────────────────────────────────

#[tokio::test]
async fn test_send_message_agent_not_running() {
    let app = create_test_app();

    // The agent isn't installed, so we get 404
    let body = serde_json::json!({
        "content": "Hello"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/agents/com.example.weather/message")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Vault keys list (empty) ───────────────────────────────────────────

#[tokio::test]
async fn test_list_vault_keys_empty() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/vault/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

// ── Config get ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_config() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["http"]["port"], 19876);
}

// ── Config update (valid) ─────────────────────────────────────────────

#[tokio::test]
async fn test_update_config_valid() {
    let app = create_test_app();

    let body = serde_json::json!({
        "log_level": "debug"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

// ── Config update (invalid log level) ─────────────────────────────────

#[tokio::test]
async fn test_update_config_invalid_log_level() {
    let app = create_test_app();

    let body = serde_json::json!({
        "log_level": "verbose"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── Install agent (missing package) ───────────────────────────────────

#[tokio::test]
async fn test_install_agent_missing_package() {
    let app = create_test_app();

    let body = serde_json::json!({
        "package_path": "/nonexistent/weather.agent"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/agents/install")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── 404 for unknown routes ────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_route_404() {
    let app = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
