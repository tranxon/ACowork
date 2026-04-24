//! Gateway Service API server (async, multi-connection)
//!
//! Accepts multiple concurrent IPC connections from Agent Runtime processes,
//! decodes requests, routes to handlers, and sends responses.
//! Each connection is handled in its own tokio task, allowing
//! multiple Agent Runtimes to communicate with the Gateway simultaneously.

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::RwLock;
#[cfg(unix)]
use tokio::sync::Mutex;

#[cfg(unix)]
use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
#[cfg(unix)]
use crate::ipc::session::SessionManager;

/// Shared state type: Arc<RwLock<GatewayState>> for concurrent read/write access.
/// RwLock chosen because handlers are predominantly read-heavy (key lookup,
/// budget query) with occasional writes (install/uninstall).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Shared session manager type
#[cfg(unix)]
type SharedSessionMgr = Arc<Mutex<SessionManager>>;

/// IPC server (async, multi-connection)
pub struct IpcServer {
    socket_path: String,
}

impl IpcServer {
    /// Create new IPC server
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Start the server (async, multi-connection)
    ///
    /// Each incoming connection is handled in its own tokio task,
    /// allowing multiple Agent Runtimes to connect concurrently.
    /// The GatewayState is protected by an async RwLock so that
    /// concurrent readers do not block each other.
    #[cfg(unix)]
    pub async fn listen(&self, state: SharedState) -> Result<(), GatewayError> {
        // Clean up stale socket file
        let _ = std::fs::remove_file(&self.socket_path);

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| {
                GatewayError::Ipc(format!(
                    "Failed to bind '{}': {}",
                    self.socket_path, e
                ))
            })?;
        tracing::info!("IPC server listening on: {}", self.socket_path);

        let session_mgr: SharedSessionMgr =
            Arc::new(Mutex::new(SessionManager::new()));
        let conn_counter = AtomicU64::new(0);

        loop {
            let (stream, _addr) = listener.accept().await.map_err(|e| {
                GatewayError::Ipc(format!("Failed to accept: {}", e))
            })?;

            let conn_id =
                format!("conn-{}", conn_counter.fetch_add(1, Ordering::Relaxed) + 1);
            tracing::info!("Accepted connection: {}", conn_id);

            let state = Arc::clone(&state);
            let session_mgr = Arc::clone(&session_mgr);

            tokio::spawn(async move {
                // Create session
                {
                    let mut mgr = session_mgr.lock().await;
                    mgr.create_session(&conn_id);
                }

                if let Err(e) =
                    handle_connection(stream, &conn_id, state, &session_mgr).await
                {
                    tracing::warn!("Connection {} error: {}", conn_id, e);
                }

                // Cleanup session on disconnect
                {
                    let mut mgr = session_mgr.lock().await;
                    mgr.remove_session(&conn_id);
                }
                tracing::info!("Connection {} closed", conn_id);
            });
        }
    }

    /// Start the server on non-Unix platforms (stub)
    #[cfg(not(unix))]
    pub async fn listen(&self, _state: SharedState) -> Result<(), GatewayError> {
        tracing::warn!(
            "IPC server: Unix sockets not available on this platform. \
             Use Named Pipe transport instead. (socket_path={})",
            self.socket_path
        );
        // On Windows, the async IPC server uses Named Pipes via transport.rs
        // The sync server below is Unix-only.
        Err(GatewayError::Ipc(
            "Async IPC server not available on non-Unix platforms. Use Named Pipe transport.".to_string()
        ))
    }
}

// ── Connection handler (Unix only) ─────────────────────────────────────────

/// Handle a single connection's request/response loop
#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    conn_id: &str,
    state: SharedState,
    session_mgr: &SharedSessionMgr,
) -> Result<(), GatewayError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);

    loop {
        let frame = match read_frame_async(&mut reader).await? {
            Some(f) => f,
            None => return Ok(()), // Connection closed
        };

        if frame.msg_type == Frame::TYPE_REQUEST {
            let request: GatewayRequest = frame.to_message().map_err(|e| {
                GatewayError::Ipc(format!("Failed to decode request: {}", e))
            })?;

            tracing::debug!("Received request from {}: {:?}", conn_id, request);

            let response =
                dispatch_request(request, conn_id, &state, session_mgr).await;

            let resp_frame =
                Frame::from_message(Frame::TYPE_RESPONSE, &response).map_err(
                    |e| GatewayError::Ipc(format!("Failed to encode response: {}", e)),
                )?;

            write_frame_async(&mut writer, &resp_frame).await?;
        }
    }
}

// ── Request dispatch ────────────────────────────────────────────────────────

/// Dispatch request to the appropriate handler
#[cfg(unix)]
#[allow(dead_code)]
async fn dispatch_request(
    request: GatewayRequest,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    match request {
        GatewayRequest::KeyRelease { provider } => {
            handle_key_release(&provider, conn_id, state, session_mgr).await
        }
        GatewayRequest::IntentSend {
            target,
            action,
            params,
            async_,
        } => {
            handle_intent_send(&target, &action, &params, async_, conn_id, state, session_mgr)
                .await
        }
        GatewayRequest::BudgetQuery { provider } => {
            handle_budget_query(&provider, state).await
        }
        GatewayRequest::UsageReport(report) => {
            handle_usage_report(report, state).await
        }
        GatewayRequest::RateAcquire { provider } => {
            handle_rate_acquire(&provider, state).await
        }
        GatewayRequest::PermissionRequest {
            permission,
            reason,
        } => handle_permission_request(&permission, &reason),
        GatewayRequest::IdentityQuery { fields } => {
            handle_identity_query(&fields, conn_id, session_mgr).await
        }
        GatewayRequest::CapabilityQuery { agent_id } => {
            handle_capability_query(agent_id.as_deref(), state).await
        }
    }
}

// ── Handler implementations ─────────────────────────────────────────────────

#[cfg(unix)]
#[allow(dead_code)]
async fn handle_key_release(
    provider: &str,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    // Check if session is authenticated (read-only on session_mgr)
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };
    // Session lock released before acquiring state lock — avoids deadlocks

    match agent_id {
        Some(id) => {
            // Read-only access to GatewayState
            let state_guard = state.read().await;
            match state_guard.vault.get_key(provider) {
                Ok(api_key) => {
                    tracing::info!(
                        "KeyRelease for agent={}, provider={}",
                        id,
                        provider
                    );
                    GatewayResponse::KeyReleaseResult {
                        api_key: Some(api_key),
                        error: None,
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "KeyRelease failed for agent={}, provider={}: {}",
                        id,
                        provider,
                        e
                    );
                    GatewayResponse::KeyReleaseResult {
                        api_key: None,
                        error: Some(e.to_string()),
                    }
                }
            }
        }
        None => {
            tracing::warn!(
                "KeyRelease from unauthenticated session {}",
                conn_id
            );
            GatewayResponse::KeyReleaseResult {
                api_key: None,
                error: Some("unauthenticated session".into()),
            }
        }
    }
}

#[cfg(unix)]
#[allow(dead_code)]
async fn handle_intent_send(
    target: &str,
    action: &str,
    _params: &serde_json::Value,
    async_: bool,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    let from = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id)
            .and_then(|s| s.agent_id.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

    tracing::info!(
        "IntentSend from={} to={} action={} async={}",
        from,
        target,
        action,
        async_
    );

    // S4.1: Generate message ID for correlation
    let message_id = format!("msg-{}", chrono::Utc::now().timestamp_millis());

    // S4.1.5: Error handling — validate target format
    if target.is_empty() {
        tracing::warn!("IntentSend rejected: empty target");
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:empty-target-{}", message_id),
        };
    }

    // S4.1.1: Check if target agent is installed
    let target_installed = {
        let guard = state.read().await;
        guard.is_installed(target)
    };

    if !target_installed {
        tracing::warn!("IntentSend rejected: agent not found: {}", target);
        // S4.1.5: AgentNotFound error — return IntentDelivered with error prefix
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:agent-not-found:{}", target),
        };
    }

    // S4.1.2: Check if target is running
    let target_running = {
        let guard = state.read().await;
        guard.is_running(target)
    };

    if !target_running {
        // S4.1.2: Target not running — need auto-spawn
        // This is coordinated by the Gateway layer (LifecycleManager)
        tracing::info!("IntentSend: target '{}' not running, auto-spawn needed", target);
    }

    // S4.1.4: For async intents, the response will be delivered via callback
    if async_ {
        tracing::info!("Async Intent queued: msg={}", message_id);
    }

    GatewayResponse::IntentDelivered { message_id }
}

/// S4.3.3: Budget query handler — returns real remaining budget
#[cfg(unix)]
#[allow(dead_code)]
async fn handle_budget_query(provider: &str, state: &SharedState) -> GatewayResponse {
    let guard = state.read().await;
    if let Some(tracker) = guard.budget_tracker() {
        let remaining = tracker.remaining_tokens(provider);
        let remaining_cost = tracker.remaining_cost_usd(provider);
        tracing::info!(
            "BudgetQuery: provider={} remaining_tokens={} remaining_cost={}",
            provider, remaining, remaining_cost
        );
        GatewayResponse::BudgetInfo {
            remaining_tokens: remaining,
            remaining_cost_usd: remaining_cost,
        }
    } else {
        // No budget tracker configured — return unlimited
        GatewayResponse::BudgetInfo {
            remaining_tokens: u64::MAX,
            remaining_cost_usd: f64::MAX,
        }
    }
}

/// S4.3.2: Usage report handler — updates cumulative usage
#[cfg(unix)]
#[allow(dead_code)]
async fn handle_usage_report(
    report: rollball_core::budget::UsageReport,
    state: &SharedState,
) -> GatewayResponse {
    tracing::info!(
        "UsageReport: agent={} provider={} tokens={} cost={:.4}",
        report.agent_id, report.provider, report.tokens_used, report.cost_usd
    );

    let mut guard = state.write().await;
    if let Some(tracker) = guard.budget_tracker_mut() {
        tracker.record_usage(
            &report.agent_id,
            &report.provider,
            report.tokens_used,
            report.cost_usd,
        );
    }

    GatewayResponse::UsageReportAck {}
}

/// S4.4.2: Rate acquire handler — token bucket allocation
#[cfg(unix)]
#[allow(dead_code)]
async fn handle_rate_acquire(provider: &str, state: &SharedState) -> GatewayResponse {
    let mut guard = state.write().await;
    if let Some(limiter) = guard.rate_limiter_mut() {
        let result = limiter.try_acquire_for(provider, "default");
        tracing::info!(
            "RateAcquire: provider={} granted={} retry_after={:?}",
            provider, result.granted, result.retry_after_ms
        );
        GatewayResponse::RateToken {
            granted: result.granted,
            retry_after_ms: result.retry_after_ms,
        }
    } else {
        // No rate limiter configured — always grant
        GatewayResponse::RateToken {
            granted: true,
            retry_after_ms: None,
        }
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn handle_permission_request(permission: &str, reason: &str) -> GatewayResponse {
    // Phase 1: always deny runtime permission requests (need user UI)
    tracing::warn!(
        "PermissionRequest denied: {} (reason: {})",
        permission,
        reason
    );
    GatewayResponse::PermissionResult {
        granted: false,
        reason: Some(
            "Runtime permission requests not supported in Phase 1".to_string(),
        ),
    }
}

/// Handle IdentityQuery request from Runtime.
///
/// S3.3/S3.4: Queries the System Agent for identity fields.
/// In Phase 2, this returns an empty result — actual query requires
/// the System Agent to be running and accessible via IPC.
#[cfg(unix)]
#[allow(dead_code)]
async fn handle_identity_query(
    fields: &[String],
    conn_id: &str,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };

    tracing::info!(
        "IdentityQuery from agent={:?}, fields={:?}",
        agent_id,
        fields
    );

    // Phase 2: Return empty result.
    // When System Agent IPC is fully connected, this will:
    // 1. Forward the query to the System Agent via Intent
    // 2. Wait for the response
    // 3. Apply PrivacyLevel filtering based on requester
    // 4. Return the filtered result
    GatewayResponse::IdentityQueryResult {
        values: std::collections::HashMap::new(),
        confidence: std::collections::HashMap::new(),
    }
}

/// Handle CapabilityQuery request from Runtime.
///
/// S4.2.4: Returns the capability registry for the requested agent
/// or all agents if no filter is specified.
#[cfg(unix)]
#[allow(dead_code)]
async fn handle_capability_query(
    agent_id: Option<&str>,
    state: &SharedState,
) -> GatewayResponse {
    let guard = state.read().await;
    let overview = guard.capability_registry.overview();

    match agent_id {
        Some(id) => {
            // Filter to specific agent
            let mut filtered = std::collections::HashMap::new();
            if let Some(actions) = overview.by_agent.get(id) {
                filtered.insert(id.to_string(), actions.clone());
            }
            tracing::info!("CapabilityQuery: agent={:?}, found={}", id, filtered.len());
            GatewayResponse::CapabilityOverview {
                capabilities: filtered,
            }
        }
        None => {
            tracing::info!("CapabilityQuery: all agents, count={}", overview.by_agent.len());
            GatewayResponse::CapabilityOverview {
                capabilities: overview.by_agent,
            }
        }
    }
}

// ── Async frame I/O helpers (Unix only) ────────────────────────────────────

/// Read a frame from an async reader
#[cfg(unix)]
async fn read_frame_async(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Option<Frame>, GatewayError> {
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; Frame::HEADER_SIZE];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(None);
        }
        Err(e) => {
            return Err(GatewayError::Ipc(format!(
                "Failed to read frame header: {}",
                e
            )));
        }
    }

    let body_len =
        u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let msg_type = header[4];

    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body).await.map_err(|e| {
        GatewayError::Ipc(format!("Failed to read frame body: {}", e))
    })?;

    Ok(Some(Frame {
        body_len: body_len as u32,
        msg_type,
        body,
    }))
}

/// Write a frame to an async writer
#[cfg(unix)]
async fn write_frame_async(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &Frame,
) -> Result<(), GatewayError> {
    use tokio::io::AsyncWriteExt;
    let bytes = frame.to_bytes();
    writer.write_all(&bytes).await.map_err(|e| {
        GatewayError::Ipc(format!("Failed to write frame: {}", e))
    })?;
    writer.flush().await.map_err(|e| {
        GatewayError::Ipc(format!("Failed to flush frame: {}", e))
    })?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(unix)]
mod unix_tests {
    use super::*;

    fn temp_socket_path(name: &str) -> String {
        format!(
            "/tmp/rollball-test-ipc-{}-{}.sock",
            name,
            std::process::id()
        )
    }

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "rollball-test-ipc-state-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn test_shared_state(name: &str) -> SharedState {
        let dir = temp_vault_dir(name);
        Arc::new(RwLock::new(GatewayState::new(&dir)))
    }

    /// Helper: send a request frame and receive a response frame over Unix stream
    async fn send_request_recv_response(
        stream: &mut tokio::net::UnixStream,
        request: &GatewayRequest,
    ) -> GatewayResponse {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let frame =
            Frame::from_message(Frame::TYPE_REQUEST, request).unwrap();
        let bytes = frame.to_bytes();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();

        // Read response header
        let mut header = [0u8; Frame::HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let body_len =
            u32::from_be_bytes([header[0], header[1], header[2], header[3]])
                as usize;
        let msg_type = header[4];

        // Read response body
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.unwrap();

        let resp_frame = Frame {
            body_len: body_len as u32,
            msg_type,
            body,
        };
        resp_frame.to_message().unwrap()
    }

    // ── Unit tests for handlers (async, with state) ──────────────────────
    
    #[tokio::test]
    async fn test_handle_budget_query() {
        let state = test_shared_state("budget-query");
        let response = handle_budget_query("openai", &state).await;
        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
            // No budget tracker configured → unlimited
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo");
        }
    }
    
    #[tokio::test]
    async fn test_handle_rate_acquire() {
        let state = test_shared_state("rate-acquire");
        let response = handle_rate_acquire("openai", &state).await;
        if let GatewayResponse::RateToken {
            granted,
            retry_after_ms,
        } = response
        {
            // No rate limiter configured → always grant
            assert!(granted);
            assert!(retry_after_ms.is_none());
        } else {
            panic!("Expected RateToken");
        }
    }
    
    #[test]
    fn test_handle_permission_request() {
        let response = handle_permission_request("filesystem:read:/etc", "need config");
        if let GatewayResponse::PermissionResult { granted, reason } = response {
            assert!(!granted);
            assert!(reason.is_some());
        } else {
            panic!("Expected PermissionResult");
        }
    }
    
    #[tokio::test]
    async fn test_handle_usage_report() {
        let state = test_shared_state("usage-report");
        let report = rollball_core::budget::UsageReport {
            agent_id: "com.example.weather".to_string(),
            provider: "openai".to_string(),
            tokens_used: 150,
            cost_usd: 0.01,
            timestamp: chrono::Utc::now(),
            error: None,
        };
        let response = handle_usage_report(report, &state).await;
        assert!(matches!(response, GatewayResponse::UsageReportAck {}));
    }

    // ── Async integration tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_ipc_server_single_connection() {
        let socket_path = temp_socket_path("single");
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("single");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        // Give server time to bind and listen
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        // Send BudgetQuery request
        let request =
            GatewayRequest::BudgetQuery { provider: "openai".to_string() };
        let response = send_request_recv_response(&mut stream, &request).await;

        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
            // No budget tracker configured → returns u64::MAX
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo, got {:?}", response);
        }

        drop(stream);
        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_ipc_server_multiple_sequential() {
        let socket_path = temp_socket_path("sequential");
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("sequential");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First connection
        {
            let mut stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let request =
                GatewayRequest::RateAcquire { provider: "openai".to_string() };
            let response =
                send_request_recv_response(&mut stream, &request).await;
            if let GatewayResponse::RateToken { granted, .. } = response {
                assert!(granted);
            } else {
                panic!("Expected RateToken");
            }
            drop(stream);
        }

        // Brief pause to let server clean up first connection
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Second connection
        {
            let mut stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let request =
                GatewayRequest::BudgetQuery { provider: "anthropic".to_string() };
            let response =
                send_request_recv_response(&mut stream, &request).await;
            if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
                assert_eq!(remaining_tokens, u64::MAX);
            } else {
                panic!("Expected BudgetInfo");
            }
            drop(stream);
        }

        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_ipc_server_concurrent_connections() {
        let socket_path = temp_socket_path("concurrent");
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("concurrent");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Spawn 10 concurrent connections, each sending a request
        let mut handles = Vec::new();
        for i in 0..10 {
            let socket_path = socket_path.clone();
            let handle = tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream =
                    tokio::net::UnixStream::connect(&socket_path).await.unwrap();

                let request = GatewayRequest::RateAcquire {
                    provider: format!("provider-{}", i),
                };
                let frame =
                    Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
                let bytes = frame.to_bytes();
                stream.write_all(&bytes).await.unwrap();
                stream.flush().await.unwrap();

                // Read response
                let mut header = [0u8; Frame::HEADER_SIZE];
                stream.read_exact(&mut header).await.unwrap();
                let body_len = u32::from_be_bytes([
                    header[0],
                    header[1],
                    header[2],
                    header[3],
                ]) as usize;
                let msg_type = header[4];
                let mut body = vec![0u8; body_len];
                stream.read_exact(&mut body).await.unwrap();

                let resp_frame = Frame {
                    body_len: body_len as u32,
                    msg_type,
                    body,
                };
                let response: GatewayResponse =
                    resp_frame.to_message().unwrap();
                response
            });
            handles.push(handle);
        }

        // All 10 should succeed
        for handle in handles {
            let response = handle.await.unwrap();
            if let GatewayResponse::RateToken { granted, .. } = response {
                assert!(granted);
            } else {
                panic!("Expected RateToken, got {:?}", response);
            }
        }

        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_gateway_state_concurrent_access() {
        let dir = temp_vault_dir("concurrent_rw");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));

        let mut handles = Vec::new();

        // Concurrent reads (should not block each other with RwLock)
        for _ in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let guard = state.read().await;
                assert!(guard.installed_agents.is_empty());
            }));
        }

        // Concurrent writes
        for i in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let mut guard = state.write().await;
                let toml_str = r#"
                    agent_id = "com.test"
                    version = "1.0.0"
                    name = "Test"
                    description = "test"
                    author = "test"
                    runtime_version = "0.1.0"
                    [llm]
                    provider = "openai"
                    model = "gpt-4"
                "#;
                let manifest =
                    rollball_core::AgentManifest::from_toml(toml_str).unwrap();
                guard.add_installed(
                    crate::gateway::state::AgentInfo {
                        agent_id: format!("com.test.{}", i),
                        version: "1.0.0".to_string(),
                        name: format!("Test Agent {}", i),
                        install_path: "/tmp/test".to_string(),
                        manifest,
                    },
                );
            }));
        }

        // All tasks should complete without deadlock
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all writes succeeded
        {
            let guard = state.read().await;
            assert_eq!(guard.installed_agents.len(), 5);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_ipc_connection_disconnect_cleanup() {
        let socket_path = temp_socket_path("disconnect");
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("disconnect");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect client A, send a request, then drop
        {
            let mut stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let request =
                GatewayRequest::BudgetQuery { provider: "openai".to_string() };
            let response =
                send_request_recv_response(&mut stream, &request).await;
            assert!(matches!(response, GatewayResponse::BudgetInfo { .. }));
            // stream dropped here
        }

        // Give server time to detect disconnect and clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect client B — server should still be healthy after A's disconnect
        {
            let mut stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let request =
                GatewayRequest::RateAcquire { provider: "anthropic".to_string() };
            let response =
                send_request_recv_response(&mut stream, &request).await;
            if let GatewayResponse::RateToken { granted, .. } = response {
                assert!(granted);
            } else {
                panic!("Expected RateToken after reconnect");
            }
        }

        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }
}
