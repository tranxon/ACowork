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
    /// Broadcast channel for CapabilityUpdate push notifications.
    /// When an agent is installed/uninstalled, a CapabilityUpdate message
    /// is broadcast to all connected Agent sessions.
    capability_tx: tokio::sync::broadcast::Sender<GatewayResponse>,
    /// Optional external session manager (shared with HTTP API)
    external_session_mgr: Option<crate::http::routes::SharedSessionMgr>,
    /// Bridge channel for forwarding Agent responses to HTTP/WebSocket clients.
    /// When an Agent sends an IntentSend targeting "http-api" or "http-ws",
    /// the response is broadcast via this channel so the HTTP WebSocket handler
    /// can stream it back to the Desktop App.
    bridge_tx: Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
}

/// Default broadcast channel capacity for capability updates
const CAPABILITY_BROADCAST_CAPACITY: usize = 64;

impl IpcServer {
    /// Create new IPC server
    pub fn new(endpoint: &str) -> Self {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory()
            .expect("Failed to create in-memory permission store");
        let (capability_tx, _) = tokio::sync::broadcast::channel(CAPABILITY_BROADCAST_CAPACITY);
        Self {
            endpoint: endpoint.to_string(),
            perm_store: Arc::new(perm_store),
            capability_tx,
            external_session_mgr: None,
            bridge_tx: None,
        }
    }

    /// Create IPC server with an existing permission store
    pub fn with_permission_store(endpoint: &str, perm_store: SharedPermissionStore) -> Self {
        let (capability_tx, _) = tokio::sync::broadcast::channel(CAPABILITY_BROADCAST_CAPACITY);
        Self {
            endpoint: endpoint.to_string(),
            perm_store,
            capability_tx,
            external_session_mgr: None,
            bridge_tx: None,
        }
    }

    /// Set external session manager (shared with HTTP API for message bridging)
    pub fn with_session_mgr(mut self, session_mgr: crate::http::routes::SharedSessionMgr) -> Self {
        self.external_session_mgr = Some(session_mgr);
        self
    }

    /// Set bridge channel for forwarding Agent responses to HTTP/WebSocket clients
    pub fn with_bridge_tx(mut self, bridge_tx: tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>) -> Self {
        self.bridge_tx = Some(bridge_tx);
        self
    }

    /// Get a sender for broadcasting capability updates.
    /// Use this from HTTP API handlers to push CapabilityUpdate
    /// to all connected Agent sessions when install/uninstall occurs.
    pub fn capability_sender(&self) -> tokio::sync::broadcast::Sender<GatewayResponse> {
        self.capability_tx.clone()
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

        // Use external session manager if provided (shared with HTTP API),
        // otherwise create a new one (standalone mode)
        let session_mgr: SharedSessionMgr =
            self.external_session_mgr.clone()
                .unwrap_or_else(|| Arc::new(Mutex::new(SessionManager::new())));
        let conn_counter = AtomicU64::new(0);
        let perm_store = Arc::clone(&self.perm_store);
        let capability_tx = self.capability_tx.clone();
        let bridge_tx = self.bridge_tx.clone();

        loop {
            let conn = server.accept().await?;

            let conn_id =
                format!("conn-{}", conn_counter.fetch_add(1, Ordering::Relaxed) + 1);
            tracing::info!("Accepted connection: {} ({})", conn_id, conn.peer_desc());

            let state = Arc::clone(&state);
            let session_mgr = Arc::clone(&session_mgr);
            let perm_store = Arc::clone(&perm_store);
            let cap_rx = capability_tx.subscribe();
            let bridge_tx = bridge_tx.clone();

            tokio::spawn(async move {
                // Session is created inside handle_connection with push channel

                if let Err(e) =
                    handle_connection(conn, &conn_id, state, &session_mgr, &perm_store, cap_rx, bridge_tx).await
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

/// Handle a single connection's request/response loop.
///
/// Uses a `tokio::select!` to multiplex:
/// 1. Incoming requests from the Agent (recv_frame)
/// 2. Server-push messages (IntentReceived)
/// 3. CapabilityUpdate broadcast messages
async fn handle_connection(
    mut conn: Box<dyn AsyncTransportConnection>,
    conn_id: &str,
    state: SharedState,
    session_mgr: &SharedSessionMgr,
    perm_store: &SharedPermissionStore,
    mut cap_rx: tokio::sync::broadcast::Receiver<GatewayResponse>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) -> Result<(), RollballError> {
    // Create server-push channel for this connection
    let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(32);

    // Register session with push channel
    {
        let mut mgr = session_mgr.lock().await;
        mgr.create_session_with_push(conn_id, push_tx);
    }

    loop {
        tokio::select! {
            // Branch 1: Incoming request from Agent
            frame_result = conn.recv_frame() => {
                let frame = match frame_result? {
                    Some(f) => f,
                    None => return Ok(()), // Connection closed
                };

                if frame.msg_type == Frame::TYPE_REQUEST {
                    let request: GatewayRequest = frame.to_message().map_err(|e| {
                        RollballError::Ipc(format!("Failed to decode request: {}", e))
                    })?;

                    tracing::debug!("Received request from {}: {:?}", conn_id, request);

                    let response =
                        dispatch_request(request, conn_id, &state, session_mgr, perm_store, &bridge_tx).await;

                    let resp_frame =
                        Frame::from_message(Frame::TYPE_RESPONSE, &response)
                            .map_err(|e| RollballError::Ipc(format!("Failed to encode response: {}", e)))?;

                    conn.send_frame(&resp_frame).await?;
                } else if frame.msg_type == Frame::TYPE_STREAM_CHUNK {
                    // IPC streaming protocol upgrade (Option B):
                    // Runtime sends TYPE_STREAM_CHUNK frames for high-frequency
                    // streaming deltas. Gateway processes them (broadcast to bridge
                    // channel) but does NOT send a response frame back, eliminating
                    // per-chunk request-response overhead.
                    let request: GatewayRequest = frame.to_message().map_err(|e| {
                        RollballError::Ipc(format!("Failed to decode stream chunk: {}", e))
                    })?;

                    tracing::trace!("Received stream chunk from {}", conn_id);

                    // Dispatch without awaiting a response — just broadcast and continue
                    let _ = dispatch_stream_chunk(request, conn_id, &state, session_mgr, &bridge_tx).await;
                    // No conn.send_frame() — no response for stream chunks
                }
            }
            // Branch 2: Server-push message (IntentReceived)
            push_msg = push_rx.recv() => {
                match push_msg {
                    Some(msg) => {
                        tracing::debug!("Server-push to {}: {:?}", conn_id, msg);
                        let push_frame =
                            Frame::from_message(Frame::TYPE_RESPONSE, &msg)
                                .map_err(|e| RollballError::Ipc(format!("Failed to encode push: {}", e)))?;
                        conn.send_frame(&push_frame).await?;
                    }
                    None => {
                        // Push channel closed — should not happen normally
                        tracing::warn!("Push channel closed for {}", conn_id);
                        return Ok(());
                    }
                }
            }
            // Branch 3: CapabilityUpdate broadcast (install/uninstall)
            cap_msg = cap_rx.recv() => {
                match cap_msg {
                    Ok(msg) => {
                        tracing::debug!("CapabilityUpdate broadcast to {}: {:?}", conn_id, msg);
                        let cap_frame =
                            Frame::from_message(Frame::TYPE_RESPONSE, &msg)
                                .map_err(|e| RollballError::Ipc(format!("Failed to encode capability update: {}", e)))?;
                        conn.send_frame(&cap_frame).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("CapabilityUpdate channel lagged for {}: skipped {} messages", conn_id, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // All senders dropped — no more capability updates
                        tracing::info!("CapabilityUpdate channel closed for {}", conn_id);
                    }
                }
            }
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
    bridge_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
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
            handle_intent_send(&target, &action, &params, async_, conn_id, state, session_mgr, perm_store, bridge_tx)
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
            request_id,
            permission,
            reason,
            timeout_ms,
        } => handle_permission_request(&request_id, &permission, &reason, timeout_ms, conn_id, state, session_mgr, perm_store).await,
        GatewayRequest::IdentityQuery { fields } => {
            handle_identity_query(&fields, conn_id, session_mgr).await
        }
        GatewayRequest::CapabilityQuery { agent_id } => {
            handle_capability_query(agent_id.as_deref(), state).await
        }
        GatewayRequest::CronRegister {
            agent_id,
            schedule,
            action,
            params,
        } => handle_cron_register(&agent_id, &schedule, &action, &params, state).await,
        GatewayRequest::CronUnregister { cron_id } => {
            handle_cron_unregister(&cron_id, state).await
        }
        GatewayRequest::CronList {} => {
            handle_cron_list(conn_id, session_mgr, state).await
        }
        GatewayRequest::AgentHello { agent_id, version, connection_role } => {
            handle_agent_hello(&agent_id, &version, &connection_role, conn_id, state, session_mgr).await
        }
    }
}

/// Dispatch a TYPE_STREAM_CHUNK request.
///
/// Lightweight handler for streaming chunk frames — extracts the IntentSend body,
/// resolves the sender's agent_id, and broadcasts to the bridge channel.
/// No response is sent back to the Runtime (that's the whole point of TYPE_STREAM_CHUNK).
/// No permission or capability checks — those were already validated by the
/// initial IntentSend that started the conversation.
async fn dispatch_stream_chunk(
    request: GatewayRequest,
    conn_id: &str,
    _state: &SharedState,
    session_mgr: &SharedSessionMgr,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) {
    // Only IntentSend makes sense as a stream chunk
    let (target, action, params) = match request {
        GatewayRequest::IntentSend {
            target,
            action,
            params,
            async_,
        } => {
            let _ = async_; // streaming chunks are always async
            (target, action, params)
        }
        _ => {
            tracing::warn!("Ignoring non-IntentSend stream chunk from {}", conn_id);
            return;
        }
    };

    // Resolve agent_id from session
    let from = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id)
            .and_then(|s| s.agent_id.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

    // Only handle HTTP bridge targets (http-api / http-ws) for streaming chunks
    if target != "http-api" && target != "http-ws" {
        tracing::debug!(
            "Ignoring stream chunk with non-HTTP target: from={} to={}",
            from, target
        );
        return;
    }

    let message_id = format!("msg-{}", chrono::Utc::now().timestamp_millis());

    // Broadcast to bridge channel — same logic as handle_intent_send's HTTP path
    if let Some(tx) = bridge_tx {
        let event_type = crate::http::routes::BridgeEventType::from_action(&action)
            .unwrap_or_else(crate::http::routes::BridgeEventType::default_for_unknown);

        let payload = match event_type {
            crate::http::routes::BridgeEventType::Chunk => {
                let delta = params.get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({ "delta": delta })
            }
            crate::http::routes::BridgeEventType::Done => {
                let content = params.get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({ "content": content })
            }
            crate::http::routes::BridgeEventType::Error => {
                let msg = params.get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown error");
                serde_json::json!({ "message": msg })
            }
            _ => params.clone(),
        };

        let event = crate::http::routes::BridgeEvent {
            agent_id: from.clone(),
            message_id: message_id.clone(),
            event_type,
            payload,
        };

        if let Err(e) = tx.send(event) {
            tracing::debug!("Failed to broadcast stream chunk: {}", e);
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

/// Maximum params size for Intent messages (64KB)
const INTENT_PARAMS_MAX_SIZE_BYTES: usize = 64 * 1024;

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
async fn handle_intent_send(
    target: &str,
    action: &str,
    params: &serde_json::Value,
    async_: bool,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
    perm_store: &SharedPermissionStore,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
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

    // Special handling: target is the HTTP/WebSocket client (not an Agent)
    // When an Agent sends a response back to the Desktop App, it targets
    // "http-api" or "http-ws". We forward via the bridge channel instead
    // of routing through the normal Intent system.
    if target == "http-api" || target == "http-ws" {
        tracing::info!(
            "IntentSend to HTTP client: from={} action={} msg={}",
            from, action, message_id
        );

        if let Some(tx) = bridge_tx {
            // Determine event type based on action
            let event_type = crate::http::routes::BridgeEventType::from_action(action)
                .unwrap_or_else(crate::http::routes::BridgeEventType::default_for_unknown);

            // Transform payload to match frontend WebSocket protocol expectations:
            //   chunk  → { "delta": "..." }
            //   done   → { "content": "..." }
            //   error  → { "message": "..." }
            //   tool_call / tool_result → pass through as-is
            let payload = match event_type {
                crate::http::routes::BridgeEventType::Chunk => {
                    let delta = params.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    serde_json::json!({ "delta": delta })
                }
                crate::http::routes::BridgeEventType::Done => {
                    // Include the full response content for 'done' events
                    let content = params.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    serde_json::json!({ "content": content })
                }
                crate::http::routes::BridgeEventType::Error => {
                    let msg = params.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    serde_json::json!({ "message": msg })
                }
                // tool_call / tool_result — pass through params as-is
                _ => params.clone(),
            };

            let event = crate::http::routes::BridgeEvent {
                agent_id: from.clone(),
                message_id: message_id.clone(),
                event_type,
                payload,
            };

            if let Err(e) = tx.send(event) {
                tracing::warn!("Failed to broadcast bridge event: {}", e);
            }
        } else {
            tracing::warn!("No bridge channel available for HTTP response");
        }

        return GatewayResponse::IntentDelivered {
            message_id: message_id.clone(),
        };
    }

    // S2.4: Intent permission check — sender must hold intent:send permission
    let intent_send_perm = Permission::IntentSend(Some(target.to_string()));
    let intent_send_broad = Permission::IntentSend(None); // broad: intent:send to any

    let has_intent_perm = perm_store.has_permission(&from, &intent_send_perm)
        .unwrap_or_else(|_| {
            // Also check broad permission (intent:send without target restriction)
            perm_store.has_permission(&from, &intent_send_broad).unwrap_or(false)
        });

    if !has_intent_perm {
        // Broad permission may cover the narrow one
        let has_broad = perm_store.has_permission(&from, &intent_send_broad).unwrap_or(false);
        if !has_broad {
            // P1-8 fix: Structured audit log for permission denial
            tracing::warn!(
                event = "permission_denied",
                permission = "intent:send",
                agent_id = %from,
                target = %target,
                action = %action,
                "Intent blocked by permission check"
            );
            return GatewayResponse::IntentDelivered {
                message_id: format!("error:permission-denied:intent:send:{}", target),
            };
        }
    }

    // S2.4: Params size limit (64KB)
    let params_size = params.to_string().len();
    if params_size > INTENT_PARAMS_MAX_SIZE_BYTES {
        tracing::warn!(
            "IntentSend rejected: params too large ({} bytes, max {} bytes)",
            params_size, INTENT_PARAMS_MAX_SIZE_BYTES
        );
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:params-too-large:{}bytes", params_size),
        };
    }

    // S2.4: Capability match check — target must declare the requested action
    let capability_match = {
        let guard = state.read().await;
        guard.capability_registry.has_action(target, action)
    };
    if !capability_match {
        tracing::warn!(
            "IntentSend rejected: target '{}' does not declare action '{}'",
            target, action
        );
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:capability-not-found:{}:{}", target, action),
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
    } else {
        // S4.1.3: Target is running — push IntentReceived to target Agent
        let target_conn_id = {
            let mgr = session_mgr.lock().await;
            mgr.find_by_agent_id(target).map(|(conn_id, _)| conn_id.clone())
        };

        if let Some(target_conn) = target_conn_id {
            let pushed = {
                let mgr = session_mgr.lock().await;
                if let Some(session) = mgr.get_session(&target_conn) {
                    let intent_msg = GatewayResponse::IntentReceived {
                        from: from.clone(),
                        action: action.to_string(),
                        params: params.clone(),
                    };
                    session.push_message(intent_msg).await
                } else {
                    false
                }
            };

            if pushed {
                tracing::info!(
                    "Intent forwarded: from={} to={} action={} via conn={}",
                    from, target, action, target_conn
                );
            } else {
                tracing::warn!(
                    "Intent push failed: target {} conn {} channel closed",
                    target, target_conn
                );
            }
        } else {
            tracing::warn!(
                "Intent target '{}' is running but has no IPC session",
                target
            );
        }
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

/// User approval callback for permission requests (S2.2).
///
/// This is the synchronous callback interface from Phase 3.
/// Phase 5 (Desktop App) will replace this with a GUI dialog via trait abstraction.
pub trait PermissionApprovalCallback: Send + Sync {
    /// Ask the user whether to grant a permission.
    /// Returns true if approved, false if denied.
    fn request_approval(&self, agent_id: &str, permission: &str, reason: &str) -> bool;
}

/// Default CLI-based approval callback (auto-deny in non-interactive mode).
///
/// S5.1: With `interactive-cli` feature, uses `dialoguer::Confirm` to
/// prompt the user. Without the feature, auto-denies as before.
pub struct CliApprovalCallback;

impl PermissionApprovalCallback for CliApprovalCallback {
    fn request_approval(&self, agent_id: &str, permission: &str, reason: &str) -> bool {
        #[cfg(feature = "interactive-cli")]
        {
            use dialoguer::Confirm;
            let prompt = format!(
                "\n\n  [Permission] Agent '{}' requests: {}\n  Reason: {}\n\n  Grant?",
                agent_id, permission, reason
            );
            Confirm::new()
                .with_prompt(prompt)
                .default(false)
                .interact()
                .unwrap_or(false)
        }

        #[cfg(not(feature = "interactive-cli"))]
        {
            tracing::warn!(
                "Permission request auto-denied (non-interactive): agent={}, perm={}, reason={}",
                agent_id, permission, reason
            );
            false
        }
    }
}

/// S2.2: Async permission approval callback.
///
/// This trait is used for approval mechanisms that may take time
/// (e.g., Desktop App GUI dialog, interactive CLI prompt).
/// The callback is spawned as a separate tokio task so that
/// the IPC main loop is not blocked while waiting for user input.
///
/// A timeout is applied — if the user does not respond within
/// the timeout, the request is automatically denied.
#[async_trait::async_trait]
pub trait AsyncPermissionApprovalCallback: Send + Sync {
    /// Ask the user whether to grant a permission.
    /// Returns true if approved, false if denied.
    /// The implementor should respect the timeout (the caller will
    /// also enforce a timeout as a safety net).
    async fn request_approval(
        &self,
        agent_id: &str,
        permission: &str,
        reason: &str,
        timeout_ms: u64,
    ) -> bool;
}

/// S2.2: Default async callback that wraps the synchronous CLI callback.
///
/// S5.1: With `interactive-cli` feature, spawns the `dialoguer` prompt
/// in `tokio::task::spawn_blocking()` to avoid blocking the IPC main loop.
/// Without the feature, auto-denies as before.
pub struct AsyncCliApprovalCallback;

#[async_trait::async_trait]
impl AsyncPermissionApprovalCallback for AsyncCliApprovalCallback {
    async fn request_approval(
        &self,
        agent_id: &str,
        permission: &str,
        reason: &str,
        _timeout_ms: u64,
    ) -> bool {
        #[cfg(feature = "interactive-cli")]
        {
            // Spawn blocking task for interactive stdin prompt.
            // This ensures the IPC handler task can continue processing
            // other connections while waiting for user input.
            let agent_id = agent_id.to_string();
            let permission = permission.to_string();
            let reason = reason.to_string();
            tokio::task::spawn_blocking(move || {
                use dialoguer::Confirm;
                let prompt = format!(
                    "\n\n  [Permission] Agent '{}' requests: {}\n  Reason: {}\n\n  Grant?",
                    agent_id, permission, reason
                );
                Confirm::new()
                    .with_prompt(prompt)
                    .default(false)
                    .interact()
                    .unwrap_or(false)
            }).await.unwrap_or(false)
        }

        #[cfg(not(feature = "interactive-cli"))]
        {
            let callback = CliApprovalCallback;
            callback.request_approval(agent_id, permission, reason)
        }
    }
}

/// S2.2: Handle runtime permission request.
///
/// Steps:
/// 1. Resolve agent_id from session
/// 2. Parse the requested permission
/// 3. Check if already granted in PermissionStore → auto-approve
/// 4. Check policy for auto-approval
/// 5. Ask the user via callback (currently auto-denies in non-interactive mode)
/// 6. Return PermissionResult with request_id for correlation
///
/// Note: The Gateway processes this in the IPC handler task. For S2.2,
/// when we implement interactive CLI approval, the approval step will
/// be spawned as a separate tokio task to avoid blocking the IPC loop.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
async fn handle_permission_request(
    request_id: &str,
    permission: &str,
    reason: &str,
    timeout_ms: u64,
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
                request_id: request_id.to_string(),
                granted: false,
                reason: Some("Not authenticated".to_string()),
            };
        }
    };

    // 2. Parse the requested permission
    let requested = match Permission::parse(permission) {
        Ok(p) => p,
        Err(e) => {
            return GatewayResponse::PermissionResult {
                request_id: request_id.to_string(),
                granted: false,
                reason: Some(format!("Invalid permission: {}", e)),
            };
        }
    };

    // 3. Check if already granted in PermissionStore
    match perm_store.has_permission(&agent_id, &requested) {
        Ok(true) => {
            tracing::info!("Permission already granted: agent={}, perm={}", agent_id, permission);
            return GatewayResponse::PermissionResult {
                request_id: request_id.to_string(),
                granted: true,
                reason: None,
            };
        }
        Ok(false) => {} // Not yet granted, continue to approval
        Err(e) => {
            return GatewayResponse::PermissionResult {
                request_id: request_id.to_string(),
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
                request_id: request_id.to_string(),
                granted: false,
                reason: Some(format!("Failed to persist grant: {}", e)),
            };
        }
        tracing::info!("Permission auto-approved: agent={}, perm={}", agent_id, permission);
        return GatewayResponse::PermissionResult {
            request_id: request_id.to_string(),
            granted: true,
            reason: None,
        };
    }

    // 5. Ask the user via async callback (S2.2)
    //
    // The async callback is spawned in a separate tokio task with a timeout.
    // This ensures the IPC handler task does not block other connections
    // while waiting for user input. The timeout is enforced both by the
    // callback implementation and as a safety net here.
    let callback = AsyncCliApprovalCallback;
    let approval_result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        callback.request_approval(&agent_id, permission, reason, timeout_ms),
    ).await;

    let approved = match approval_result {
        Ok(approved) => approved,
        Err(_) => {
            // Timeout — auto-deny
            tracing::warn!(
                "Permission request timed out ({}ms): agent={}, perm={}",
                timeout_ms, agent_id, permission
            );
            return GatewayResponse::PermissionResult {
                request_id: request_id.to_string(),
                granted: false,
                reason: Some(format!("Permission request timed out after {}ms", timeout_ms)),
            };
        }
    };

    if approved {
        // Persist the grant
        let grant = PermissionGrant::new(&agent_id, requested.clone(), "user");
        if let Err(e) = perm_store.grant(&grant) {
            return GatewayResponse::PermissionResult {
                request_id: request_id.to_string(),
                granted: false,
                reason: Some(format!("Failed to persist grant: {}", e)),
            };
        }
        GatewayResponse::PermissionResult {
            request_id: request_id.to_string(),
            granted: true,
            reason: None,
        }
    } else {
        GatewayResponse::PermissionResult {
            request_id: request_id.to_string(),
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

// ── Cron handlers (S3.4) ──────────────────────────────────────────────────

async fn handle_cron_register(
    agent_id: &str,
    schedule: &str,
    action: &str,
    params: &serde_json::Value,
    state: &SharedState,
) -> GatewayResponse {
    let (cron_id, store_clone) = {
        let mut guard = state.write().await;
        match guard.cron_scheduler.register(agent_id, schedule, action, params.clone()) {
            Ok(id) => {
                let store = guard.cron_store.clone();
                (id, store)
            }
            Err(e) => {
                tracing::warn!("Cron register failed: agent={} schedule={} error={}", agent_id, schedule, e);
                return GatewayResponse::CronRegisterResult {
                    cron_id: None,
                    error: Some(e),
                };
            }
        }
    };

    // P1-9 fix: Use spawn_blocking for synchronous rusqlite operations
    if let Some(store) = store_clone {
        let entry = crate::cron::StoredCronEntry {
            id: cron_id.clone(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params: serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string()),
        };
        let cron_id_clone = cron_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.insert(&entry) {
                tracing::warn!("Failed to persist cron entry {}: {}", cron_id_clone, e);
            }
        }).await;
    }

    tracing::info!(
        "Cron registered via IPC: agent={} cron_id={} schedule={} action={}",
        agent_id, cron_id, schedule, action
    );
    GatewayResponse::CronRegisterResult {
        cron_id: Some(cron_id),
        error: None,
    }
}

async fn handle_cron_unregister(
    cron_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    let (removed, store_clone) = {
        let mut guard = state.write().await;
        let removed = guard.cron_scheduler.unregister(cron_id);
        let store = guard.cron_store.clone();
        (removed, store)
    };

    // P1-9 fix: Use spawn_blocking for synchronous rusqlite operations
    if removed
        && let Some(store) = store_clone
    {
        let cron_id_clone = cron_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.delete(&cron_id_clone) {
                tracing::warn!("Failed to delete cron entry {} from store: {}", cron_id_clone, e);
            }
        }).await;
    }

    tracing::info!("Cron unregister: cron_id={} removed={}", cron_id, removed);
    GatewayResponse::CronUnregisterResult { removed }
}

async fn handle_cron_list(
    conn_id: &str,
    session_mgr: &SharedSessionMgr,
    state: &SharedState,
) -> GatewayResponse {
    // Get agent_id from session
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };

    let agent_id = match agent_id {
        Some(id) => id,
        None => {
            return GatewayResponse::CronListResult { entries: vec![] };
        }
    };

    let guard = state.read().await;
    let entries = guard.cron_scheduler
        .entries_for_agent(&agent_id)
        .into_iter()
        .map(|e| rollball_core::protocol::CronEntryInfo {
            id: e.id.clone(),
            agent_id: e.agent_id.clone(),
            schedule: e.schedule.clone(),
            action: e.action.clone(),
            params: e.params.clone(),
        })
        .collect();

    GatewayResponse::CronListResult { entries }
}

/// Handle AgentHello — register the session with the agent's identity
///
/// On successful authentication, also pushes LLMConfigDelivery to the Agent
/// via the session's push channel. This satisfies PRD GTW-05 and SEC-07:
/// API keys are distributed via IPC, not environment variables.
async fn handle_agent_hello(
    agent_id: &str,
    version: &str,
    connection_role: &str,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    tracing::info!(
        "AgentHello received: agent_id={} version={} conn={} role={}",
        agent_id, version, conn_id, connection_role
    );

    let mut mgr = session_mgr.lock().await;
    if let Some(session) = mgr.get_session_mut(conn_id) {
        session.authenticate(agent_id);
        session.connection_role = connection_role.to_string();
        tracing::info!("Session {} authenticated as agent {} (role={})", conn_id, agent_id, connection_role);

        // Only push LLM config to main connections.
        // chunk-relay connections don't need LLM config — they only send TYPE_STREAM_CHUNK.
        if connection_role == "main" {
            let llm_config = resolve_llm_config_for_agent(agent_id, state).await;
        if let Some((provider, model, api_key, base_url, models)) = llm_config {
            tracing::info!(
                "Pushing LLMConfigDelivery to agent={}: provider={} model={:?} models={:?}",
                agent_id, provider, model, models
            );
            // Use push_message (async) to deliver LLM config via the session's push channel
            let push_result = session.push_message(GatewayResponse::LLMConfigDelivery {
                provider,
                model,
                api_key,
                base_url,
                models,
            }).await;
            if !push_result {
                tracing::warn!("Failed to push LLMConfigDelivery to {} (channel closed)", conn_id);
            }
        } else {
            tracing::warn!(
                "No LLM config available for agent={}. Agent will fall back to manifest/env.",
                agent_id
            );
        }
        } // end if connection_role == "main"

        GatewayResponse::AgentHelloResult {
            success: true,
            error: None,
        }
    } else {
        tracing::warn!("AgentHello from unknown connection {}", conn_id);
        GatewayResponse::AgentHelloResult {
            success: false,
            error: Some(format!("Unknown connection: {}", conn_id)),
        }
    }
}

/// Resolve the LLM configuration to deliver to an Agent.
///
/// Priority:
/// 1. Gateway config `default_provider` + `default_model` → look up in Vault
/// 2. First key stored in Vault (with its default_model)
/// 3. None (Agent falls back to manifest suggested_provider + env vars)
///
/// Model resolution order (within the chosen provider):
/// 1. Gateway config `default_model` (explicit user choice)
/// 2. Vault entry's `default_model` (set when adding the provider key)
/// 3. None — Agent Runtime falls back to its manifest's suggested_model
pub async fn resolve_llm_config_for_agent(
    _agent_id: &str,
    state: &SharedState,
) -> Option<(String, Option<String>, String, Option<String>, Vec<String>)> {
    let state_guard = state.read().await;

    // Try default_provider from Gateway config first
    let default_provider = state_guard.config.as_ref()
        .and_then(|c| c.default_provider.as_deref());

    // Try default_model from Gateway config
    let config_default_model = state_guard.config.as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Determine which provider to use
    let provider_name = if let Some(name) = default_provider {
        Some(name.to_string())
    } else {
        // Fall back to first key in Vault
        state_guard.vault.list_providers().first().cloned()
    };

    let provider_name = match provider_name {
        Some(name) => name,
        None => {
            tracing::info!("No provider configured in Vault, cannot deliver LLM config");
            return None;
        }
    };

    // Retrieve the provider entry from Vault
    match state_guard.vault.get_provider(&provider_name) {
        Ok(entry) => {
            // Model resolution: config default > Vault default > None (Agent decides)
            let model = config_default_model
                .map(|m| m.to_string())
                .or(entry.default_model.clone());
            // model is None when neither config nor Vault has a preference —
            // Agent Runtime will fall back to its manifest's suggested_model

            Some((
                provider_name.clone(),
                model,
                entry.api_key,
                entry.base_url,
                entry.models,
            ))
        }
        Err(e) => {
            tracing::warn!("Failed to get provider '{}' from Vault: {}", provider_name, e);
            None
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
            "req-test-1",
            "filesystem:read:/etc",
            "need config",
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;
        if let GatewayResponse::PermissionResult { request_id, granted, reason } = response {
            assert_eq!(request_id, "req-test-1");
            assert!(!granted);
            assert!(reason.is_some());
        } else {
            panic!("Expected PermissionResult");
        }
    }

    /// S2.2: Test permission auto-approved when already granted in store
    #[tokio::test]
    async fn test_handle_permission_already_granted() {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory().unwrap();
        let shared_perm_store: SharedPermissionStore = Arc::new(perm_store);
        let state = test_shared_state("perm-granted");
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

        // Create an authenticated session
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-1", push_tx);
            mgr.get_session_mut("conn-1")
                .unwrap()
                .authenticate("com.example.agent");
        }

        // Pre-grant filesystem:read:/etc permission
        let grant = PermissionGrant::new(
            "com.example.agent",
            Permission::FilesystemRead(Some("/etc".to_string())),
            "user",
        );
        shared_perm_store.grant(&grant).unwrap();

        let response = handle_permission_request(
            "req-auto-1",
            "filesystem:read:/etc",
            "need config",
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;

        if let GatewayResponse::PermissionResult { request_id, granted, reason } = response {
            assert_eq!(request_id, "req-auto-1");
            assert!(granted, "Should be auto-approved from store");
            assert!(reason.is_none());
        } else {
            panic!("Expected PermissionResult");
        }
    }

    /// S2.2: Test permission auto-approved by policy (MemoryRead = Allow)
    #[tokio::test]
    async fn test_handle_permission_policy_allow() {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory().unwrap();
        let shared_perm_store: SharedPermissionStore = Arc::new(perm_store);
        let state = test_shared_state("perm-policy");
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

        // Create an authenticated session
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-1", push_tx);
            mgr.get_session_mut("conn-1")
                .unwrap()
                .authenticate("com.example.agent");
        }

        // MemoryRead is auto-approved by policy — no pre-grant needed
        let response = handle_permission_request(
            "req-policy-1",
            "memory:read",
            "need memory access",
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;

        if let GatewayResponse::PermissionResult { request_id, granted, reason: _ } = response {
            assert_eq!(request_id, "req-policy-1");
            assert!(granted, "MemoryRead should be auto-approved by policy");
        } else {
            panic!("Expected PermissionResult");
        }
    }

    /// S2.2: Test permission denied by non-interactive CLI (Shell = AskAlways)
    #[tokio::test]
    async fn test_handle_permission_denied_noninteractive() {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory().unwrap();
        let shared_perm_store: SharedPermissionStore = Arc::new(perm_store);
        let state = test_shared_state("perm-denied");
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

        // Create an authenticated session
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-1", push_tx);
            mgr.get_session_mut("conn-1")
                .unwrap()
                .authenticate("com.example.agent");
        }

        // Shell requires AskAlways policy — non-interactive CLI auto-denies
        let response = handle_permission_request(
            "req-shell-1",
            "shell",
            "need shell access",
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;

        if let GatewayResponse::PermissionResult { request_id, granted, reason } = response {
            assert_eq!(request_id, "req-shell-1");
            assert!(!granted, "Shell should be denied in non-interactive mode");
            assert!(reason.is_some());
        } else {
            panic!("Expected PermissionResult");
        }
    }

    /// S2.2: Test invalid permission string
    #[tokio::test]
    async fn test_handle_permission_invalid_string() {
        let perm_store = crate::permission_store::PermissionStore::open_in_memory().unwrap();
        let shared_perm_store: SharedPermissionStore = Arc::new(perm_store);
        let state = test_shared_state("perm-invalid");
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

        // Create an authenticated session
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-1", push_tx);
            mgr.get_session_mut("conn-1")
                .unwrap()
                .authenticate("com.example.agent");
        }

        let response = handle_permission_request(
            "req-invalid-1",
            "invalid:permission:format",
            "testing invalid",
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
            "conn-1",
            &state,
            &session_mgr,
            &shared_perm_store,
        ).await;

        if let GatewayResponse::PermissionResult { request_id, granted, reason } = response {
            assert_eq!(request_id, "req-invalid-1");
            assert!(!granted);
            assert!(reason.unwrap().contains("Invalid permission"));
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

    /// S4.1.3: Test that IntentSend pushes IntentReceived to the target's session
    #[tokio::test]
    async fn test_intent_push_to_target_session() {
        let dir = temp_vault_dir("intent_push");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));
        let _perm_store: SharedPermissionStore =
            Arc::new(crate::permission_store::PermissionStore::open_in_memory().unwrap());

        // Grant intent:send permission to sender
        let intent_grant = PermissionGrant::new(
            "com.example.sender",
            Permission::IntentSend(None), // broad: can send to any target
            "test",
        );
        _perm_store.grant(&intent_grant).unwrap();

        // Register target's capability
        {
            let mut guard = state.write().await;
            guard.capability_registry.register(
                "com.example.target",
                "weather_query",
                rollball_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Simulate target agent's session with a push channel
        let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-target", push_tx);
            mgr.get_session_mut("conn-target")
                .unwrap()
                .authenticate("com.example.target");
        }

        // Mark target as installed and running
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
            guard.add_running(crate::gateway::state::RunningAgentInfo {
                agent_id: "com.example.target".to_string(),
                pid: 1234,
                started_at: chrono::Utc::now(),
                workspace: "/tmp/test".to_string(),
            });
        }

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        // Call handle_intent_send
        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &serde_json::json!({"city": "Shanghai"}),
            false,
            "conn-sender",
            &state,
            &session_mgr,
            &_perm_store,
            &None,
        )
        .await;

        // Verify the immediate response is IntentDelivered
        match &response {
            GatewayResponse::IntentDelivered { message_id } => {
                assert!(!message_id.starts_with("error:"));
            }
            _ => panic!("Expected IntentDelivered, got {:?}", response),
        }

        // Verify the target received IntentReceived via push channel
        let pushed_msg = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            push_rx.recv(),
        )
        .await
        .expect("Timeout waiting for push message")
        .expect("Push channel closed");

        match &pushed_msg {
            GatewayResponse::IntentReceived {
                from,
                action,
                params,
            } => {
                assert_eq!(from, "com.example.sender");
                assert_eq!(action, "weather_query");
                assert_eq!(params["city"], "Shanghai");
            }
            _ => panic!("Expected IntentReceived, got {:?}", pushed_msg),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2.4: Test IntentSend rejected when sender lacks intent:send permission
    #[tokio::test]
    async fn test_intent_send_no_permission() {
        let dir = temp_vault_dir("intent_no_perm");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));
        let perm_store: SharedPermissionStore =
            Arc::new(crate::permission_store::PermissionStore::open_in_memory().unwrap());

        // Install and register target with capability
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
            guard.capability_registry.register(
                "com.example.target",
                "weather_query",
                rollball_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Simulate sender's session (no intent:send permission granted)
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &serde_json::json!({"city": "Shanghai"}),
            false,
            "conn-sender",
            &state,
            &session_mgr,
            &perm_store,
            &None,
        )
        .await;

        if let GatewayResponse::IntentDelivered { message_id } = &response {
            assert!(
                message_id.starts_with("error:permission-denied"),
                "Expected permission denied error, got: {}",
                message_id
            );
        } else {
            panic!("Expected IntentDelivered with error, got {:?}", response);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2.4: Test IntentSend rejected when target lacks the requested capability
    #[tokio::test]
    async fn test_intent_send_capability_mismatch() {
        let dir = temp_vault_dir("intent_no_cap");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));
        let perm_store: SharedPermissionStore =
            Arc::new(crate::permission_store::PermissionStore::open_in_memory().unwrap());

        // Install target (but don't register any capability)
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
        }

        // Grant intent:send permission to sender
        let intent_grant = PermissionGrant::new(
            "com.example.sender",
            Permission::IntentSend(None),
            "test",
        );
        perm_store.grant(&intent_grant).unwrap();

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        let response = handle_intent_send(
            "com.example.target",
            "nonexistent_action",
            &serde_json::json!({}),
            false,
            "conn-sender",
            &state,
            &session_mgr,
            &perm_store,
            &None,
        )
        .await;

        if let GatewayResponse::IntentDelivered { message_id } = &response {
            assert!(
                message_id.starts_with("error:capability-not-found"),
                "Expected capability-not-found error, got: {}",
                message_id
            );
        } else {
            panic!("Expected IntentDelivered with error, got {:?}", response);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2.4: Test IntentSend rejected when params exceed 64KB limit
    #[tokio::test]
    async fn test_intent_send_params_too_large() {
        let dir = temp_vault_dir("intent_large_params");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));
        let perm_store: SharedPermissionStore =
            Arc::new(crate::permission_store::PermissionStore::open_in_memory().unwrap());

        // Install target with capability
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
            guard.capability_registry.register(
                "com.example.target",
                "weather_query",
                rollball_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Grant intent:send permission
        let intent_grant = PermissionGrant::new(
            "com.example.sender",
            Permission::IntentSend(None),
            "test",
        );
        perm_store.grant(&intent_grant).unwrap();

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        // Create params > 64KB
        let large_data = "x".repeat(65 * 1024);
        let large_params = serde_json::json!({"data": large_data});

        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &large_params,
            false,
            "conn-sender",
            &state,
            &session_mgr,
            &perm_store,
            &None,
        )
        .await;

        if let GatewayResponse::IntentDelivered { message_id } = &response {
            assert!(
                message_id.starts_with("error:params-too-large"),
                "Expected params-too-large error, got: {}",
                message_id
            );
        } else {
            panic!("Expected IntentDelivered with error, got {:?}", response);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn test_capability_broadcast_to_sessions() {
        let (capability_tx, mut cap_rx1) =
            tokio::sync::broadcast::channel::<GatewayResponse>(CAPABILITY_BROADCAST_CAPACITY);
        let mut cap_rx2 = capability_tx.subscribe();

        // Simulate an install event — broadcast CapabilityUpdate
        let update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec!["query".to_string(), "forecast".to_string()],
            removed: false,
        };
        capability_tx.send(update.clone()).unwrap();

        // Both subscribers should receive the update
        let msg1 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx1.recv(),
        )
        .await
        .expect("Timeout waiting for broadcast on subscriber 1")
        .expect("Channel closed");

        let msg2 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx2.recv(),
        )
        .await
        .expect("Timeout waiting for broadcast on subscriber 2")
        .expect("Channel closed");

        match (&msg1, &msg2) {
            (
                GatewayResponse::CapabilityUpdate { agent_id, actions, removed },
                GatewayResponse::CapabilityUpdate { .. },
            ) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(actions.len(), 2);
                assert!(!removed);
            }
            _ => panic!("Expected CapabilityUpdate, got {:?} and {:?}", msg1, msg2),
        }

        // Simulate an uninstall event
        let remove_update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec![],
            removed: true,
        };
        capability_tx.send(remove_update.clone()).unwrap();

        let msg3 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx1.recv(),
        )
        .await
        .expect("Timeout waiting for uninstall broadcast")
        .expect("Channel closed");

        match &msg3 {
            GatewayResponse::CapabilityUpdate { agent_id, actions, removed } => {
                assert_eq!(agent_id, "com.example.weather");
                assert!(actions.is_empty());
                assert!(*removed);
            }
            _ => panic!("Expected CapabilityUpdate (removed), got {:?}", msg3),
        }
    }
}
