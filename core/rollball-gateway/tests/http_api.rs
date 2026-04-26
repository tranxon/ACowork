//! HTTP API integration tests
//!
//! Tests all HTTP API endpoints using Axum's test framework (tower::ServiceExt).

use axum::body::Body;
use axum::http::{Request, Method, StatusCode};
use tower::ServiceExt; // for oneshot()

use rollball_gateway::http::routes::{AppState, build_router, BridgeEvent, SharedSessionMgr};
use rollball_gateway::http::auth::HttpAuth;
use rollball_gateway::gateway::state::GatewayState;
use rollball_gateway::ipc::session::SessionManager;

fn create_test_app() -> axum::Router {
    let dir = std::env::temp_dir().join(format!(
        "rollball-test-http-api-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut gw_state = GatewayState::new(&dir.to_string_lossy());
    // P0-2 fix: Inject config so get_config() returns real data
    gw_state.config = Some(rollball_gateway::config::GatewayConfig::default());
    let state = AppState {
        gateway_state: std::sync::Arc::new(tokio::sync::RwLock::new(gw_state)),
        auth: std::sync::Arc::new(HttpAuth::new(false)),
        session_mgr: None,
        bridge_tx: None,
    };
    build_router(state)
}

fn create_test_app_with_session() -> (axum::Router, SharedSessionMgr, tokio::sync::broadcast::Sender<BridgeEvent>) {
    let dir = std::env::temp_dir().join(format!(
        "rollball-test-http-api-session-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut gw_state = GatewayState::new(&dir.to_string_lossy());
    gw_state.config = Some(rollball_gateway::config::GatewayConfig::default());
    let session_mgr: SharedSessionMgr = std::sync::Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
    let (bridge_tx, _) = tokio::sync::broadcast::channel::<BridgeEvent>(256);

    let state = AppState {
        gateway_state: std::sync::Arc::new(tokio::sync::RwLock::new(gw_state)),
        auth: std::sync::Arc::new(HttpAuth::new(false)),
        session_mgr: Some(session_mgr.clone()),
        bridge_tx: Some(bridge_tx.clone()),
    };
    (build_router(state), session_mgr, bridge_tx)
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

// ── Bridge event forwarding ────────────────────────────────────────────

#[tokio::test]
async fn test_bridge_event_serialization() {
    let event = BridgeEvent {
        agent_id: "com.example.weather".to_string(),
        message_id: "msg-abc".to_string(),
        event_type: "chunk".to_string(),
        payload: serde_json::json!({"delta": "Hello"}),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("com.example.weather"));
    assert!(json.contains("chunk"));
    assert!(json.contains("msg-abc"));
}

#[tokio::test]
async fn test_bridge_channel_broadcast() {
    let (bridge_tx, mut bridge_rx) = tokio::sync::broadcast::channel::<BridgeEvent>(256);

    let event = BridgeEvent {
        agent_id: "com.example.weather".to_string(),
        message_id: "msg-001".to_string(),
        event_type: "done".to_string(),
        payload: serde_json::json!({"usage": {"tokens": 150}}),
    };

    bridge_tx.send(event.clone()).unwrap();

    let received = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        bridge_rx.recv(),
    )
    .await
    .expect("Timeout")
    .expect("Channel closed");

    assert_eq!(received.agent_id, "com.example.weather");
    assert_eq!(received.event_type, "done");
    assert_eq!(received.message_id, "msg-001");
}

// ── Send message to agent with session (not running) ────────────────────

#[tokio::test]
async fn test_send_message_agent_installed_not_running() {
    let (app, session_mgr, _bridge_tx) = create_test_app_with_session();

    // Register agent in session manager but not in gateway state
    // This simulates an agent that was installed but has no running process
    {
        let mut mgr = session_mgr.lock().await;
        mgr.create_session_with_push(
            "conn-1",
            tokio::sync::mpsc::channel(32).0,
        );
        mgr.get_session_mut("conn-1")
            .unwrap()
            .authenticate("com.example.weather");
    }

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

    // Agent not in gateway state → NOT_FOUND
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Config update with both fields ──────────────────────────────────────

#[tokio::test]
async fn test_update_config_both_fields() {
    let app = create_test_app();

    let body = serde_json::json!({
        "log_level": "warn",
        "idle_timeout_secs": 600
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

    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["message"].as_str().unwrap().contains("log_level=warn"));
    assert!(json["message"].as_str().unwrap().contains("idle_timeout_secs=600"));
}

// ── Config update with empty body ──────────────────────────────────────

#[tokio::test]
async fn test_update_config_empty_body() {
    let app = create_test_app();

    let body = serde_json::json!({});

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
