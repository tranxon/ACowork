//! Gateway Service API server (async, multi-connection, platform-agnostic)
//!
//! Accepts multiple concurrent IPC connections from Agent Runtime processes,
//! decodes requests, routes to handlers, and sends responses.
//!
//! Each connection is handled in its own tokio task, allowing
//! multiple Agent Runtimes to communicate with the Gateway simultaneously.
//!
//! Platform-specific transport (Unix Socket / Named Pipe) is injected via
//! `AsyncTransportServer` / `AsyncTransportConnection` traits.
//! This file contains ZERO `#[cfg(unix)]` / `#[cfg(windows)]` annotations.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, Mutex};

use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use rollball_core::transport::AsyncTransportConnection;
use rollball_core::error::RollballError;
use rollball_core::permission::{Permission, PermissionGrant, PermissionPolicy};
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::ipc::session::SessionManager;
use crate::ipc::transport;

/// Shared state type: Arc<RwLock<GatewayState>> for concurrent read/write access.
/// RwLock chosen because handlers are predominantly read-heavy (key lookup,
/// budget query) with occasional writes (install/uninstall).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Shared permission store type.
/// PermissionStore internally uses Mutex<Connection> for thread safety.
pub type SharedPermissionStore = Arc<crate::permission_store::PermissionStore>;

/// Shared session manager type
type SharedSessionMgr = Arc<Mutex<SessionManager>>;

/// IPC server (async, multi-connection, platform-agnostic)
pub struct IpcServer {
    endpoint: String,
    perm_store: SharedPermissionStore,
}

impl IpcServer {
    /// Create new IPC server
    pub fn new(endpoint: &str) -> Self {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory()
            .expect("Failed to create in-memory permission store");
        Self {
            endpoint: endpoint.to_string(),
            perm_store: Arc::new(perm_store),
        }
    }

    /// Create IPC server with an existing permission store
    pub fn with_permission_store(endpoint: &str, perm_store: SharedPermissionStore) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            perm_store,
        }
    }

    /// Start the server (async, multi-connection)
    ///
    /// Each incoming connection is handled in its own tokio task,
    /// allowing multiple Agent Runtimes to connect concurrently.
    /// The GatewayState is protected by an async RwLock so that
    /// concurrent readers do not block each other.
    pub async fn listen(&self, state: SharedState) -> Result<(), GatewayError> {
        let mut server = transport::create_server(&self.endpoint)?;

        server.listen().await?;

        tracing::info!("IPC server listening on: {}", server.endpoint_desc());

        let session_mgr: SharedSessionMgr =
            Arc::new(Mutex::new(SessionManager::new()));
        let conn_counter = AtomicU64::new(0);
        let perm_store = Arc::clone(&self.perm_store);

        loop {
            let conn = server.accept().await?;

            let conn_id =
                format!("conn-{}", conn_counter.fetch_add(1, Ordering::Relaxed) + 1);
            tracing::info!("Accepted connection: {} ({})", conn_id, conn.peer_desc());

            let state = Arc::clone(&state);
            let session_mgr = Arc::clone(&session_mgr);
            let perm_store = Arc::clone(&perm_store);

            tokio::spawn(async move {
                // Create session
                {
                    let mut mgr = session_mgr.lock().await;
                    mgr.create_session(&conn_id);
                }

                if let Err(e) =
                    handle_connection(conn, &conn_id, state, &session_mgr, &perm_store).await
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
}

// ── Connection handler (platform-agnostic) ─────────────────────────────────

/// Handle a single connection's request/response loop
async fn handle_connection(
    mut conn: Box<dyn AsyncTransportConnection>,
    conn_id: &str,
    state: SharedState,
    session_mgr: &SharedSessionMgr,
    perm_store: &SharedPermissionStore,
) -> Result<(), RollballError> {
    loop {
        let frame = match conn.recv_frame().await? {
            Some(f) => f,
            None => return Ok(()), // Connection closed
        };

        if frame.msg_type == Frame::TYPE_REQUEST {
            let request: GatewayRequest = frame.to_message().map_err(|e| {
                RollballError::Ipc(format!("Failed to decode request: {}", e))
            })?;

            tracing::debug!("Received request from {}: {:?}", conn_id, request);

            let response =
                dispatch_request(request, conn_id, &state, session_mgr, perm_store).await;

            let resp_frame =
                Frame::from_message(Frame::TYPE_RESPONSE, &response)
                    .map_err(|e| RollballError::Ipc(format!("Failed to encode response: {}", e)))?;

            conn.send_frame(&resp_frame).await?;
        }
    }
}

// ── Request dispatch ────────────────────────────────────────────────────────

/// Dispatch request to the appropriate handler
#[allow(dead_code)]
async fn dispatch_request(
    request: GatewayRequest,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
    perm_store: &SharedPermissionStore,
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
        } => handle_permission_request(&permission, &reason, conn_id, state, session_mgr, perm_store).await,
        GatewayRequest::IdentityQuery { fields } => {
            handle_identity_query(&fields, conn_id, session_mgr).await
        }
        GatewayRequest::CapabilityQuery { agent_id } => {
            handle_capability_query(agent_id.as_deref(), state).await
        }
    }
}

// ── Handler implementations ─────────────────────────────────────────────────

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

/// User approval callback for permission requests.
///
/// In Phase 3, this is a CLI-style callback that prints to stdout and reads from stdin.
/// Phase 5 (Desktop App) will replace this with a GUI dialog via trait abstraction.
pub trait PermissionApprovalCallback: Send + Sync {
    /// Ask the user whether to grant a permission.
    /// Returns true if approved, false if denied.
    fn request_approval(&self, agent_id: &str, permission: &str, reason: &str) -> bool;
}

/// Default CLI-based approval callback (auto-deny in non-interactive mode).
pub struct CliApprovalCallback;

impl PermissionApprovalCallback for CliApprovalCallback {
    fn request_approval(&self, agent_id: &str, permission: &str, reason: &str) -> bool {
        // Phase 3: Non-interactive mode — log and auto-deny.
        // Interactive CLI mode will be implemented when the CLI is fully built.
        tracing::warn!(
            "Permission request auto-denied (non-interactive): agent={}, perm={}, reason={}",
            agent_id, permission, reason
        );
        false
    }
}

#[allow(dead_code)]
async fn handle_permission_request(
    permission: &str,
    reason: &str,
    conn_id: &str,
    _state: &SharedState,
    session_mgr: &SharedSessionMgr,
    perm_store: &SharedPermissionStore,
) -> GatewayResponse {
    // 1. Resolve agent_id from session
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };
    let agent_id = match agent_id {
        Some(id) => id,
        None => {
            return GatewayResponse::PermissionResult {
                granted: false,
                reason: Some("Not authenticated".to_string()),
            };
        }
    };

    // 2. Parse the requested permission
    let requested = match Permission::parse(permission) {
        Some(p) => p,
        None => {
            return GatewayResponse::PermissionResult {
                granted: false,
                reason: Some(format!("Invalid permission string: {}", permission)),
            };
        }
    };

    // 3. Check if already granted in PermissionStore
    match perm_store.has_permission(&agent_id, &requested) {
        Ok(true) => {
            tracing::info!("Permission already granted: agent={}, perm={}", agent_id, permission);
            return GatewayResponse::PermissionResult {
                granted: true,
                reason: None,
            };
        }
        Ok(false) => {} // Not yet granted, continue to approval
        Err(e) => {
            return GatewayResponse::PermissionResult {
                granted: false,
                reason: Some(format!("Permission store error: {}", e)),
            };
        }
    }

    // 4. Check policy for auto-approval
    let policy = PermissionPolicy::for_permission(&requested);
    if policy == PermissionPolicy::Allow {
        // Auto-approve and persist
        let grant = PermissionGrant::new(&agent_id, requested.clone(), "auto");
        if let Err(e) = perm_store.grant(&grant) {
            return GatewayResponse::PermissionResult {
                granted: false,
                reason: Some(format!("Failed to persist grant: {}", e)),
            };
        }
        tracing::info!("Permission auto-approved: agent={}, perm={}", agent_id, permission);
        return GatewayResponse::PermissionResult {
            granted: true,
            reason: None,
        };
    }

    // 5. Ask the user (via callback — currently auto-denies)
    let callback = CliApprovalCallback;
    let approved = callback.request_approval(&agent_id, permission, reason);

    if approved {
        // Persist the grant
        let grant = PermissionGrant::new(&agent_id, requested.clone(), "user");
        if let Err(e) = perm_store.grant(&grant) {
            return GatewayResponse::PermissionResult {
                granted: false,
                reason: Some(format!("Failed to persist grant: {}", e)),
            };
        }
        GatewayResponse::PermissionResult {
            granted: true,
            reason: None,
        }
    } else {
        GatewayResponse::PermissionResult {
            granted: false,
            reason: Some(format!("User denied permission: {}", permission)),
        }
    }
}

/// Handle IdentityQuery request from Runtime.
///
/// S3.3/S3.4: Queries the System Agent for identity fields.
/// In Phase 2, this returns an empty result — actual query requires
/// the System Agent to be running and accessible via IPC.
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn test_handle_permission_request() {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory().unwrap();
        let shared_perm_store: SharedPermissionStore = Arc::new(perm_store);
        let state = test_shared_state("perm-request");
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));
        // No session created → should return "Not authenticated"
        let response = handle_permission_request(
            "filesystem:read:/etc",
            "need config",
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;
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

    // ── Integration tests (platform-specific transport) ──────────────────

    /// Helper: send a request frame and receive a response frame
    async fn send_request_recv_response(
        conn: &mut dyn AsyncTransportConnection,
        request: &GatewayRequest,
    ) -> GatewayResponse {
        let frame =
            Frame::from_message(Frame::TYPE_REQUEST, request).unwrap();
        conn.send_frame(&frame).await.unwrap();

        let resp_frame = conn.recv_frame().await.unwrap().unwrap();
        resp_frame.to_message().unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_ipc_server_single_connection() {
        let socket_path = format!(
            "/tmp/rollball-test-ipc-single-{}.sock",
            std::process::id()
        );
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("single");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        // Give server time to bind and listen
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();
        let mut conn = Box::new(
            crate::ipc::transport::unix_transport::UnixTransportConnection::new(stream)
        );

        // Send BudgetQuery request
        let request =
            GatewayRequest::BudgetQuery { provider: "openai".to_string() };
        let response = send_request_recv_response(&mut *conn, &request).await;

        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo, got {:?}", response);
        }

        drop(conn);
        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_ipc_server_multiple_sequential() {
        let socket_path = format!(
            "/tmp/rollball-test-ipc-seq-{}.sock",
            std::process::id()
        );
        let _ = std::fs::remove_file(&socket_path);
        let state = test_shared_state("sequential");

        let server = IpcServer::new(&socket_path);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First connection
        {
            let stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let mut conn = Box::new(
                crate::ipc::transport::unix_transport::UnixTransportConnection::new(stream)
            );
            let request =
                GatewayRequest::RateAcquire { provider: "openai".to_string() };
            let response =
                send_request_recv_response(&mut *conn, &request).await;
            if let GatewayResponse::RateToken { granted, .. } = response {
                assert!(granted);
            } else {
                panic!("Expected RateToken");
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Second connection
        {
            let stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .unwrap();
            let mut conn = Box::new(
                crate::ipc::transport::unix_transport::UnixTransportConnection::new(stream)
            );
            let request =
                GatewayRequest::BudgetQuery { provider: "anthropic".to_string() };
            let response =
                send_request_recv_response(&mut *conn, &request).await;
            if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
                assert_eq!(remaining_tokens, u64::MAX);
            } else {
                panic!("Expected BudgetInfo");
            }
        }

        server_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_ipc_server_concurrent_connections() {
        let socket_path = format!(
            "/tmp/rollball-test-ipc-conc-{}.sock",
            std::process::id()
        );
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
                let stream =
                    tokio::net::UnixStream::connect(&socket_path).await.unwrap();
                let mut conn: Box<dyn AsyncTransportConnection> = Box::new(
                    crate::ipc::transport::unix_transport::UnixTransportConnection::new(stream)
                );

                let request = GatewayRequest::RateAcquire {
                    provider: format!("provider-{}", i),
                };
                let frame =
                    Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
                conn.send_frame(&frame).await.unwrap();

                let resp_frame = conn.recv_frame().await.unwrap().unwrap();
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

    #[cfg(windows)]
    #[tokio::test]
    async fn test_ipc_server_named_pipe_single() {
        let pipe_name = format!(
            r"\\.\pipe\rollball-test-ipc-{}",
            std::process::id()
        );
        let state = test_shared_state("np-single");

        let server = IpcServer::new(&pipe_name);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        // Give server time to reach first accept() and create pipe instance
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut conn = crate::ipc::transport::windows_transport::connect_client_async(&pipe_name)
            .await
            .expect("Failed to connect to Named Pipe");

        let request = GatewayRequest::BudgetQuery { provider: "openai".to_string() };
        let response = send_request_recv_response(&mut *conn, &request).await;

        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo, got {:?}", response);
        }

        drop(conn);
        server_handle.abort();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_ipc_server_named_pipe_multiple_sequential() {
        let pipe_name = format!(
            r"\\.\pipe\rollball-test-ipc-seq-{}",
            std::process::id()
        );
        let state = test_shared_state("np-seq");

        let server = IpcServer::new(&pipe_name);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // First connection
        {
            let mut conn = crate::ipc::transport::windows_transport::connect_client_async(&pipe_name)
                .await
                .expect("Failed to connect to Named Pipe (1st)");
            let request = GatewayRequest::RateAcquire { provider: "openai".to_string() };
            let response = send_request_recv_response(&mut *conn, &request).await;
            if let GatewayResponse::RateToken { granted, .. } = response {
                assert!(granted);
            } else {
                panic!("Expected RateToken, got {:?}", response);
            }
            drop(conn);
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Second connection
        {
            let mut conn = crate::ipc::transport::windows_transport::connect_client_async(&pipe_name)
                .await
                .expect("Failed to connect to Named Pipe (2nd)");
            let request = GatewayRequest::BudgetQuery { provider: "anthropic".to_string() };
            let response = send_request_recv_response(&mut *conn, &request).await;
            if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
                assert_eq!(remaining_tokens, u64::MAX);
            } else {
                panic!("Expected BudgetInfo, got {:?}", response);
            }
            drop(conn);
        }

        server_handle.abort();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_ipc_server_named_pipe_concurrent() {
        let pipe_name = format!(
            r"\\.\pipe\rollball-test-ipc-conc-{}",
            std::process::id()
        );
        let state = test_shared_state("np-conc");

        let server = IpcServer::new(&pipe_name);
        let server_handle = tokio::spawn(async move { server.listen(state).await });

        // Give the server time to start and create the first pipe instance
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Spawn 10 concurrent connections, each sending a request.
        // Uses connect_client_async which retries on NotFound/Busy.
        let mut handles = Vec::new();
        for i in 0..10 {
            let pipe_name = pipe_name.clone();
            let handle = tokio::spawn(async move {
                let mut conn: Box<dyn AsyncTransportConnection> =
                    crate::ipc::transport::windows_transport::connect_client_async(&pipe_name)
                        .await
                        .expect("Failed to connect to Named Pipe");

                let request = GatewayRequest::RateAcquire {
                    provider: format!("provider-{}", i),
                };
                let frame =
                    Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
                conn.send_frame(&frame).await.unwrap();

                let resp_frame = conn.recv_frame().await.unwrap().unwrap();
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
}
