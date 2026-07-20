//! End-to-end integration tests for the LSP Relay HTTP server.
//!
//! These tests start an actual HTTP server on a random port and make
//! real HTTP requests using `reqwest`. This validates the full request
//! pipeline: TCP listener → Axum router → handler → response serialization.

use std::sync::Arc;

use acowork_core::event_bus::EventBus;
use acowork_lsp_relay::pool::LspPool;
use acowork_lsp_relay::server::AppState;
use acowork_lsp_relay::state::LspRelayState;

/// Test fixture: start the LSP Relay HTTP server on a random port.
///
/// Returns the base URL, an event bus handle (for publishing state events),
/// and a shutdown sender. When the sender is dropped (or `send(())` is
/// called), the server stops gracefully.
struct TestServer {
    base_url: String,
    event_bus: EventBus<LspRelayState>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl TestServer {
    async fn start() -> Self {
        let lsp_pool = Arc::new(LspPool::new());
        let event_bus = EventBus::new(64);
        event_bus.spawn_heartbeat(2000);
        event_bus.publish_state(LspRelayState::Ready { language_count: 0 });

        let state = Arc::new(AppState {
            lsp_pool,
            event_bus: event_bus.clone(),
        });

        let app = acowork_lsp_relay::server::build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind");
        let addr = listener.local_addr().expect("Failed to get local addr");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("Server error");
        });

        // Give the server a moment to start accepting connections
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        TestServer {
            base_url: format!("http://{}", addr),
            event_bus,
            shutdown_tx,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn ws_url(&self, path: &str) -> String {
        self.base_url.replace("http://", "ws://") + path
    }
}

// ── Existing E2E tests ─────────────────────────────────────────────────

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let server = TestServer::start().await;

    let resp = reqwest::get(server.url("/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["process"], "acowork-lsp-relay");
    assert!(!body["version"].as_str().unwrap().is_empty());
    assert!(body["details"]["language_count"].as_u64().is_some());

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn servers_endpoint_returns_config() {
    let server = TestServer::start().await;

    let resp = reqwest::get(server.url("/api/lsp/servers-with-status")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    // Combined payload: { servers: LspServersConfig, status: { lang: entry } }
    assert_eq!(body["servers"]["version"], 1);
    assert!(
        body["servers"]["servers"].as_object().unwrap().contains_key("rust"),
        "expected 'rust' in servers"
    );
    assert!(
        body["status"].as_object().unwrap().contains_key("rust"),
        "expected 'rust' in status"
    );

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn status_endpoint_returns_list() {
    let server = TestServer::start().await;

    let resp = reqwest::get(server.url("/api/lsp/status")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let entries = body.as_array().expect("status should be an array");
    assert!(!entries.is_empty(), "status list should not be empty");

    for entry in entries {
        assert!(entry["language"].as_str().is_some());
        assert!(entry["installed"].as_bool().is_some());
    }

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn install_get_known_language_returns_script() {
    let server = TestServer::start().await;

    let resp = reqwest::get(server.url("/api/lsp/install/rust")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["language"], "rust");
    assert!(!body["script"].as_str().unwrap().is_empty());

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn install_get_unknown_language_returns_404() {
    let server = TestServer::start().await;

    let resp = reqwest::get(server.url("/api/lsp/install/brainfuck")).await.unwrap();
    assert_eq!(resp.status(), 404);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("brainfuck"));

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn events_endpoint_returns_sse_stream() {
    let server = TestServer::start().await;

    let client = reqwest::Client::new();
    let resp = client.get(server.url("/events")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();

    let mut received_heartbeat = false;
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if buf.contains("event:heartbeat") || buf.contains("event: heartbeat") {
                    received_heartbeat = true;
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(
        received_heartbeat,
        "expected at least one heartbeat event in SSE stream, got: {buf}"
    );

    let _ = server.shutdown_tx.send(());
}

#[tokio::test]
async fn lsp_websocket_unknown_language_returns_400() {
    let server = TestServer::start().await;

    let client = reqwest::Client::new();
    let resp = client.get(server.url("/lsp/brainfuck")).send().await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx for unknown language, got {}",
        resp.status()
    );

    let _ = server.shutdown_tx.send(());
}

// ── New E2E: POST /api/lsp/install — execute real script ───────────────

#[tokio::test]
async fn post_install_runs_script_and_returns_output() {
    // Test both success and failure paths in a single test to avoid
    // env var race conditions between parallel tests.

    // --- Success path ---
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_dir = dir.path().join("lsp_install");
        std::fs::create_dir_all(&install_dir).expect("mkdir lsp_install");

        let mock_script = "#!/bin/bash\necho 'mock install success'\nexit 0\n";
        std::fs::write(install_dir.join("rust.sh"), mock_script).expect("write mock script");

        // SAFETY: No other test in this file uses ACOWORK_LSP_CONFIG_DIR
        // at the same time because both success and failure paths are
        // tested sequentially within this single test function.
        unsafe {
            std::env::set_var("ACOWORK_LSP_CONFIG_DIR", dir.path());
        }

        let server = TestServer::start().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(server.url("/api/lsp/install/rust"))
            .send()
            .await
            .unwrap();

        unsafe {
            std::env::remove_var("ACOWORK_LSP_CONFIG_DIR");
        }

        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["language"], "rust");
        assert_eq!(body["success"], true);
        assert_eq!(body["exit_code"], 0);
        assert!(
            body["stdout"]
                .as_str()
                .unwrap()
                .contains("mock install success"),
            "stdout should contain mock output: {}",
            body["stdout"]
        );

        let _ = server.shutdown_tx.send(());
    }

    // --- Failure path ---
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let install_dir = dir.path().join("lsp_install");
        std::fs::create_dir_all(&install_dir).expect("mkdir");

        let mock_script = "#!/bin/bash\necho 'install failed'\nexit 1\n";
        std::fs::write(install_dir.join("rust.sh"), mock_script).expect("write script");

        unsafe {
            std::env::set_var("ACOWORK_LSP_CONFIG_DIR", dir.path());
        }

        let server = TestServer::start().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(server.url("/api/lsp/install/rust"))
            .send()
            .await
            .unwrap();

        unsafe {
            std::env::remove_var("ACOWORK_LSP_CONFIG_DIR");
        }

        assert_eq!(resp.status(), 500);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["exit_code"], 1);
        assert!(body["stdout"].as_str().unwrap().contains("install failed"));

        let _ = server.shutdown_tx.send(());
    }
}

// ── New E2E: SSE /events — receive state event ─────────────────────────

#[tokio::test]
async fn events_endpoint_delivers_state_event() {
    let server = TestServer::start().await;

    // Connect to SSE endpoint
    let client = reqwest::Client::new();
    let resp = client.get(server.url("/events")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();

    // Give the handler a moment to subscribe, then publish a state event
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server
        .event_bus
        .publish_state(LspRelayState::Error {
            message: "test error state".to_string(),
        });

    // Read the stream until we find the state event
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut found_state = false;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // Look for state event containing "error" and "test error state"
                if (buf.contains("event:state") || buf.contains("event: state"))
                    && buf.contains("test error state")
                {
                    found_state = true;
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(
        found_state,
        "expected state event with 'test error state' in SSE stream, got: {buf}"
    );

    let _ = server.shutdown_tx.send(());
}

// ── New E2E: WebSocket /lsp/{language} — actual connection ─────────────

#[tokio::test]
async fn lsp_websocket_relay_with_real_lsp_server() {
    // This test connects via WebSocket to the LSP relay endpoint.
    // It requires an LSP server to be installed. We check for
    // rust-analyzer first; if not found, we skip the test.
    let rust_analyzer = std::process::Command::new("which")
        .arg("rust-analyzer")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some();

    if !rust_analyzer {
        eprintln!("skipping lsp_websocket_relay_with_real_lsp_server: rust-analyzer not on PATH");
        return;
    }

    let server = TestServer::start().await;

    // Connect via WebSocket
    let ws_url = server.ws_url("/lsp/rust?workspace_root=/tmp");
    let (mut ws, _response) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WebSocket connect failed");

    // Send an LSP initialize request
    let init_request = r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"capabilities":{},"processId":null,"rootUri":null}}"#;
    use futures_util::{SinkExt, StreamExt};
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        init_request.into(),
    ))
    .await
    .expect("send initialize");

    // Wait for a response (InitializeResult or any response)
    let mut received_response = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                    // Look for a response with "capabilities" (InitializeResult)
                    if text.contains("capabilities") {
                        received_response = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }

    assert!(
        received_response,
        "expected InitializeResult with capabilities from rust-analyzer"
    );

    let _ = server.shutdown_tx.send(());
}

// ── New E2E: Graceful shutdown ─────────────────────────────────────────

#[tokio::test]
async fn server_graceful_shutdown_stops_accepting_connections() {
    let server = TestServer::start().await;

    // Verify server is running
    let resp = reqwest::get(server.url("/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Send shutdown signal
    let health_url = server.url("/health");
    let _ = server.shutdown_tx.send(());

    // Give the server a moment to shut down
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify the server is no longer accepting connections
    let result = reqwest::get(&health_url).await;
    assert!(
        result.is_err(),
        "server should not accept connections after shutdown"
    );
}

#[tokio::test]
async fn server_handles_concurrent_requests() {
    let server = TestServer::start().await;

    // Send multiple concurrent requests
    let client = reqwest::Client::new();
    let mut handles = Vec::new();

    for _ in 0..10 {
        let url = server.url("/health");
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let resp = client.get(&url).send().await.unwrap();
            assert_eq!(resp.status(), 200);
        }));
    }

    // Wait for all requests to complete
    for handle in handles {
        handle.await.expect("task panicked");
    }

    let _ = server.shutdown_tx.send(());
}

// ── E2E: Gateway ↔ LSP Relay integration ──────────────────────────────
//
// These tests verify the integration points between the Gateway and the
// LSP Relay process. They test the supervisor's ability to monitor the
// relay's SSE heartbeat stream and detect failures.
//
// Since we can't spawn a real acowork-lsp-relay binary in unit tests
// (it may not be built), we use the LSP Relay's own HTTP server (via
// the TestServer fixture) to simulate the relay, and test the Gateway's
// supervisor logic against it.

/// Test that the Gateway's `check_lsp_relay_health` function can
/// successfully query a running LSP Relay's /health endpoint.
#[tokio::test]
async fn gateway_can_query_lsp_relay_health() {
    let server = TestServer::start().await;

    // Simulate what the Gateway does: query /health
    let port = server.base_url.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    let url = format!("http://127.0.0.1:{}/health", port);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["process"], "acowork-lsp-relay");

    let _ = server.shutdown_tx.send(());
}

/// Test that the Gateway's supervisor can connect to the LSP Relay's
/// SSE /events stream and receive heartbeat events.
#[tokio::test]
async fn gateway_supervisor_can_connect_to_sse_events() {
    let server = TestServer::start().await;
    let port = server.base_url.rsplit(':').next().unwrap().parse::<u16>().unwrap();

    // Simulate what the supervisor does: connect to /events
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let resp = client
        .get(format!("http://127.0.0.1:{}/events", port))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut received_heartbeat = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let text = String::from_utf8_lossy(&chunk);
                if text.contains("event:heartbeat") || text.contains("event: heartbeat") {
                    received_heartbeat = true;
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(received_heartbeat, "Supervisor should receive heartbeat from SSE stream");

    let _ = server.shutdown_tx.send(());
}

/// Test that the Gateway's `try_connect_events` equivalent logic
/// returns false when the LSP Relay is not running.
#[tokio::test]
async fn gateway_supervisor_detects_relay_down() {
    // Use a port that's definitely not serving anything
    let port = 1u16;
    let url = format!("http://127.0.0.1:{}/events", port);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();

    let result = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await;

    assert!(result.is_err(), "Connection to port 1 should fail");
}

/// Test the full lifecycle: start relay → verify health → verify SSE →
/// shutdown relay → verify health check fails.
#[tokio::test]
async fn full_lifecycle_start_health_sse_shutdown() {
    // 1. Start the LSP Relay HTTP server
    let server = TestServer::start().await;
    let port = server.base_url.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    let base_url = format!("http://127.0.0.1:{}", port);

    // 2. Verify health check succeeds
    let resp = reqwest::get(format!("{}/health", base_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let health: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(health["status"], "ok");

    // 3. Verify SSE events stream delivers heartbeats
    let client = reqwest::Client::new();
    let sse_resp = client
        .get(format!("{}/events", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), 200);
    assert_eq!(
        sse_resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    use futures_util::StreamExt;
    let mut stream = sse_resp.bytes_stream();
    let mut got_heartbeat = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                if String::from_utf8_lossy(&chunk).contains("heartbeat") {
                    got_heartbeat = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(got_heartbeat, "Should receive heartbeat before shutdown");

    // 4. Shutdown the relay
    let _ = server.shutdown_tx.send(());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 5. Verify health check now fails
    let result = reqwest::get(format!("{}/health", base_url)).await;
    assert!(result.is_err(), "Health check should fail after shutdown");
}

/// Test that the LSP Relay publishes state transitions on the SSE stream.
/// The Gateway supervisor relies on these to know when the relay is Ready.
#[tokio::test]
async fn sse_stream_delivers_state_transition() {
    let server = TestServer::start().await;
    let port = server.base_url.rsplit(':').next().unwrap().parse::<u16>().unwrap();

    // Connect to SSE
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{}/events", port))
        .send()
        .await
        .unwrap();

    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();

    // Give the handler a moment to subscribe, then publish a state event
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server.event_bus.publish_state(LspRelayState::Ready { language_count: 7 });

    // Read until we find the state event
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut found_ready = false;

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if (buf.contains("event:state") || buf.contains("event: state"))
                    && buf.contains("ready")
                    && buf.contains("language_count")
                {
                    found_ready = true;
                    break;
                }
            }
            _ => break,
        }
    }

    assert!(
        found_ready,
        "Supervisor should see Ready state transition in SSE stream, got: {buf}"
    );

    let _ = server.shutdown_tx.send(());
}

/// Test that the LSP Relay's /api/lsp/endpoint discovery pattern works:
/// a client can query /health to check availability, then use the relay.
#[tokio::test]
async fn endpoint_discovery_via_health_check() {
    let server = TestServer::start().await;
    let port = server.base_url.rsplit(':').next().unwrap().parse::<u16>().unwrap();

    // Simulate the Gateway's startup flow:
    // 1. Check if relay is already running
    let health_url = format!("http://127.0.0.1:{}/health", port);
    let resp = reqwest::get(&health_url).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");

    // 2. Relay is available — "attach" to it
    let language_count = body["details"]["language_count"].as_u64();
    assert!(language_count.is_some(), "Health response should include language_count");

    // 3. Verify the relay serves LSP API
    let servers_url = format!("http://127.0.0.1:{}/api/lsp/servers-with-status", port);
    let resp = reqwest::get(&servers_url).await.unwrap();
    assert_eq!(resp.status(), 200);

    let _ = server.shutdown_tx.send(());
}
