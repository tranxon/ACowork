//! Gateway gRPC server implementation.
//!
//! Implements the `GatewayService` trait from the generated proto code,
//! providing bidirectional streaming RPC via tonic. Each connection is
//! handled in its own tokio task with a `tokio::select!` loop that
//! multiplexes:
//! 1. Incoming requests from the Agent (via gRPC stream)
//! 2. Server-push messages (IntentReceived)
//! 3. CapabilityUpdate broadcast messages

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, mpsc, oneshot};
use tonic::{Request, Response, Status, Streaming};

use rollball_core::proto;
use rollball_core::proto::gateway_service_server::{GatewayService, GatewayServiceServer};
use rollball_core::proto_bridge::GatewayResponseToProto;
use rollball_core::protocol::GatewayResponse;

use crate::ipc::server::SharedState;
use crate::http::approval::ApprovalPendingRequests;
use crate::http::routes::{BridgeEvent, SessionPendingRequests, SharedSessionMgr};

use super::dispatch::{dispatch_grpc_request, is_stream_chunk};

/// Default gRPC listen port
const DEFAULT_GRPC_PORT: u16 = 19877;

// ── GrpcSession ────────────────────────────────────────────────────────────

/// Session state for a gRPC-connected Agent Runtime.
///
/// Unlike the IPC `Session`, this stores the gRPC-specific push channel
/// which sends `Result<proto::ServerMessage, Status>` items.
pub struct GrpcSession {
    /// Agent ID (set after AgentHello handshake)
    pub agent_id: Option<String>,
    /// Connection role: "main" for primary, "chunk-relay" for streaming
    pub connection_role: String,
    /// Server-push channel sender for delivering messages to this Agent.
    /// The receiver end is held by the outbound stream task.
    push_tx: mpsc::Sender<Result<proto::ServerMessage, Status>>,
    /// Whether the session has been authenticated (AgentHello completed)
    pub authenticated: bool,
}

impl GrpcSession {
    /// Create a new unauthenticated gRPC session
    pub fn new(push_tx: mpsc::Sender<Result<proto::ServerMessage, Status>>) -> Self {
        Self {
            agent_id: None,
            connection_role: "main".to_string(),
            push_tx,
            authenticated: false,
        }
    }

    /// Mark session as authenticated
    pub fn authenticate(&mut self, agent_id: &str) {
        self.agent_id = Some(agent_id.to_string());
        self.authenticated = true;
    }

    /// Try to push a domain GatewayResponse to this session's Agent.
    /// Converts to proto and sends via the gRPC outbound channel.
    /// Returns false if the channel is closed.
    pub async fn push_message(&self, msg: GatewayResponse) -> bool {
        let server_msg = msg.to_proto(0); // request_id = 0 for unsolicited push
        self.push_tx.send(Ok(server_msg)).await.is_ok()
    }

    /// Try to push a proto ServerMessage directly.
    pub async fn push_proto(&self, msg: proto::ServerMessage) -> bool {
        self.push_tx.send(Ok(msg)).await.is_ok()
    }

    /// Push a proto ServerMessage with a non-zero request_id.
    /// Used for request-response patterns where the Runtime sends a
    /// ClientMessage response with the same request_id.
    pub async fn push_request(&self, msg: proto::ServerMessage) -> bool {
        debug_assert_ne!(msg.request_id, 0, "push_request expects non-zero request_id");
        self.push_tx.send(Ok(msg)).await.is_ok()
    }

    /// Non-async version of push_request for use in sync contexts.
    /// Uses try_send on the bounded mpsc channel (capacity 32).
    /// Returns false if the channel is closed; rarely fails due to full buffer.
    pub fn try_push_request(&self, msg: proto::ServerMessage) -> bool {
        debug_assert_ne!(msg.request_id, 0, "try_push_request expects non-zero request_id");
        self.push_tx.try_send(Ok(msg)).is_ok()
    }
}

// ── GrpcSessionManager ──────────────────────────────────────────────────────

/// Manages all active gRPC sessions.
pub struct GrpcSessionManager {
    sessions: HashMap<String, GrpcSession>,
    /// Pending Gateway→Runtime requests awaiting response.
    /// Maps request_id → oneshot sender for ClientMessage response.
    pending_requests: HashMap<u64, oneshot::Sender<proto::ClientMessage>>,
    /// Reverse index: conn_id → Vec<request_id>.  Used to clean up
    /// pending requests when a session is removed (e.g. Runtime crash)
    /// so the oneshot senders don't leak.
    session_requests: HashMap<String, Vec<u64>>,
    /// Monotonically increasing request ID counter for Gateway→Runtime requests.
    next_request_id: AtomicU64,
}

impl GrpcSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            pending_requests: HashMap::new(),
            session_requests: HashMap::new(),
            next_request_id: AtomicU64::new(1),
        }
    }

    /// Create a new session with a push channel
    pub fn create_session(
        &mut self,
        conn_id: &str,
        push_tx: mpsc::Sender<Result<proto::ServerMessage, Status>>,
    ) {
        self.sessions
            .insert(conn_id.to_string(), GrpcSession::new(push_tx));
    }

    /// Get a session by connection ID
    pub fn get_session(&self, conn_id: &str) -> Option<&GrpcSession> {
        self.sessions.get(conn_id)
    }

    /// Get a mutable session by connection ID
    pub fn get_session_mut(&mut self, conn_id: &str) -> Option<&mut GrpcSession> {
        self.sessions.get_mut(conn_id)
    }

    /// Remove a session (on disconnect).
    ///
    /// Also cleans up any pending Gateway→Runtime requests associated
    /// with this session so that HTTP handlers awaiting responses don't
    /// leak memory (the dropped oneshot::Sender will wake them with a
    /// RecvError).
    pub fn remove_session(&mut self, conn_id: &str) -> Option<GrpcSession> {
        // Clean up pending requests belonging to this session.
        if let Some(request_ids) = self.session_requests.remove(conn_id) {
            for request_id in &request_ids {
                self.pending_requests.remove(request_id);
            }
            tracing::debug!(
                conn_id = %conn_id,
                count = request_ids.len(),
                "Cleaned up pending requests for removed session"
            );
        }
        self.sessions.remove(conn_id)
    }

    /// Find session by agent_id (only main connections)
    pub fn find_by_agent_id(&self, agent_id: &str) -> Option<(&String, &GrpcSession)> {
        self.sessions.iter().find(|(_, s)| {
            s.agent_id.as_deref() == Some(agent_id) && s.connection_role == "main"
        })
    }

    /// Get a mutable reference to the session for a given agent_id.
    pub fn find_by_agent_id_mut(&mut self, agent_id: &str) -> Option<&mut GrpcSession> {
        self.sessions.iter_mut().find_map(|(_, s)| {
            if s.agent_id.as_deref() == Some(agent_id) && s.connection_role == "main" {
                Some(s)
            } else {
                None
            }
        })
    }

    /// Allocate a new request_id for Gateway→Runtime requests.
    pub fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Register a pending request and return a receiver for the response.
    ///
    /// The caller should send the ServerMessage to the Runtime, then await
    /// the returned oneshot receiver. When the Runtime responds with a
    /// ClientMessage bearing the same request_id, the inbound handler will
    /// route it here.
    pub fn register_pending_request(
        &mut self,
        request_id: u64,
    ) -> oneshot::Receiver<proto::ClientMessage> {
        let (tx, rx) = oneshot::channel();
        self.pending_requests.insert(request_id, tx);
        rx
    }

    /// Try to fulfill a pending request with a response ClientMessage.
    /// Returns true if the request_id was found and fulfilled.
    pub fn fulfill_pending(&mut self, request_id: u64, msg: proto::ClientMessage) -> bool {
        if let Some(sender) = self.pending_requests.remove(&request_id) {
            let _ = sender.send(msg);
            true
        } else {
            false
        }
    }

    /// Send a memory query ServerMessage to the Runtime and return a receiver
    /// for the response.
    ///
    /// This method does NOT wait for the response. The caller MUST:
    /// 1. Drop the `&mut self` reference (release the Mutex lock)
    /// 2. Await the returned receiver with a timeout
    /// 3. On timeout, call `cleanup_pending(request_id)` to unregister
    ///
    /// Returns None if the agent is not connected or the push fails.
    pub fn send_memory_request(
        &mut self,
        agent_id: &str,
        query: proto::server_message::Payload,
    ) -> Option<(u64, oneshot::Receiver<proto::ClientMessage>)> {
        let request_id = self.next_request_id();
        let rx = self.register_pending_request(request_id);

        // Build ServerMessage with the query payload
        let server_msg = proto::ServerMessage {
            request_id,
            payload: Some(query),
        };

        // Push to the Runtime via the session's push channel.
        // This uses try_send to avoid deadlocking: push_request is async
        // but we cannot hold a sync Mutex across .await.
        // The push channel is unbounded so try_send always succeeds.
        // Look up the session and try to push.  We defer all &mut self
        // operations to after the find_by_agent_id borrow is released
        // so the borrow checker can see disjoint field access.
        let push_result: Result<String, ()> = {
            match self.find_by_agent_id(agent_id) {
                Some((conn_id, session)) => {
                    if !session.try_push_request(server_msg) {
                        tracing::warn!(
                            agent_id = %agent_id,
                            "Failed to push memory request to Runtime (channel closed)"
                        );
                        Err(())
                    } else {
                        Ok(conn_id.clone())
                    }
                }
                None => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "Agent not connected, cannot send memory request"
                    );
                    Err(())
                }
            }
        };

        match push_result {
            Ok(conn_id) => {
                // Record the conn_id → request_id mapping so that
                // remove_session() can clean up when the Runtime disconnects.
                self.session_requests
                    .entry(conn_id)
                    .or_default()
                    .push(request_id);
            }
            Err(()) => {
                self.pending_requests.remove(&request_id);
                return None;
            }
        };

        Some((request_id, rx))
    }

    /// Remove a pending request (call after timeout).
    pub fn cleanup_pending(&mut self, request_id: u64) {
        self.pending_requests.remove(&request_id);
    }
}

impl Default for GrpcSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared gRPC session manager type
pub type SharedGrpcSessionMgr = Arc<Mutex<GrpcSessionManager>>;

// ── GatewayGrpcService ──────────────────────────────────────────────────────

/// The gRPC service implementation for the Gateway.
///
/// Holds all shared state needed to process requests and push messages.
pub struct GatewayGrpcService {
    state: SharedState,
    grpc_session_mgr: SharedGrpcSessionMgr,
    /// Legacy IPC session manager — shared with IPC server for intent routing.
    /// gRPC sessions register here too so that IntentReceived push works.
    ipc_session_mgr: SharedSessionMgr,
    capability_tx: tokio::sync::broadcast::Sender<GatewayResponse>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: Option<SessionPendingRequests>,
    approval_pending: Option<ApprovalPendingRequests>,
}

#[async_trait::async_trait]
impl GatewayService for GatewayGrpcService {
    type ConnectStream =
        tokio_stream::wrappers::ReceiverStream<Result<proto::ServerMessage, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<proto::ClientMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let mut inbound = request.into_inner();

        // Create outbound channel
        let (outbound_tx, outbound_rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(32);

        // Assign a connection ID
        let conn_id = format!(
            "grpc-{}",
            CONN_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
        );

        tracing::info!("gRPC connect: new connection {}", conn_id);

        // Register gRPC session
        {
            let mut mgr = self.grpc_session_mgr.lock().await;
            mgr.create_session(&conn_id, outbound_tx.clone());
        }

        // Also register in IPC session manager so intent routing can find us.
        // We create an IPC push channel that bridges to gRPC outbound.
        let (ipc_push_tx, mut ipc_push_rx) = mpsc::channel::<GatewayResponse>(32);
        {
            let mut mgr = self.ipc_session_mgr.lock().await;
            mgr.create_session_with_push(&conn_id, ipc_push_tx);
        }

        // Clone all shared state for the spawned task
        let state = Arc::clone(&self.state);
        let grpc_session_mgr = Arc::clone(&self.grpc_session_mgr);
        let ipc_session_mgr = Arc::clone(&self.ipc_session_mgr);
        let mut cap_rx = self.capability_tx.subscribe();
        let bridge_tx = self.bridge_tx.clone();
        let session_pending = self.session_pending.clone();
        let approval_pending = self.approval_pending.clone();
        let conn_id_clone = conn_id.clone();

        // Spawn handler task for this connection
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Branch 1: Incoming request from Runtime
                    msg = inbound.message() => {
                        match msg {
                            Ok(Some(client_msg)) => {
                                let _request_id = client_msg.request_id;

                                // Intercept memory API responses from Runtime.
                                // These bypass dispatch and are routed to the
                                // pending request map for HTTP handler fulfillment.
                                if is_memory_result(&client_msg) {
                                    let mut mgr = grpc_session_mgr.lock().await;
                                    mgr.fulfill_pending(client_msg.request_id, client_msg);
                                    continue;
                                }

                                // Stream chunks: dispatch but don't send response
                                if is_stream_chunk(&client_msg) {
                                    // For stream chunks, we still dispatch for bridge forwarding
                                    let _ = dispatch_grpc_request(
                                        client_msg,
                                        &conn_id_clone,
                                        &state,
                                        &ipc_session_mgr,
                                        &bridge_tx,
                                        &session_pending,
                                        &approval_pending,
                                    ).await;
                                    continue;
                                }

                                // Intercept AgentHello to also authenticate the GrpcSession.
                                // The dispatch handler authenticates the IPC session, but the
                                // GrpcSession (used by send_memory_request → find_by_agent_id)
                                // needs its own agent_id set for memory API routing.
                                if let Some(proto::client_message::Payload::AgentHello(ref req)) = client_msg.payload {
                                    let mut grpc_mgr = grpc_session_mgr.lock().await;
                                    if let Some(session) = grpc_mgr.get_session_mut(&conn_id_clone) {
                                        session.authenticate(&req.agent_id);
                                        session.connection_role = req.connection_role.clone();
                                        tracing::info!(
                                            agent_id = %req.agent_id,
                                            conn = %conn_id_clone,
                                            "GrpcSession authenticated for memory API routing"
                                        );
                                    }
                                }

                                // Dispatch and send response
                                let server_msg = dispatch_grpc_request(
                                    client_msg,
                                    &conn_id_clone,
                                    &state,
                                    &ipc_session_mgr,
                                    &bridge_tx,
                                    &session_pending,
                                    &approval_pending,
                                ).await;

                                // For AgentHello, the handler also pushes LLMConfigDelivery
                                // and WorkspaceContextUpdate via the IPC session's push channel.
                                // We need to deliver those to the gRPC outbound too.
                                // This is handled by Branch 2 (ipc_push_rx).

                                if server_msg.payload.is_some() {
                                    let _ = outbound_tx.send(Ok(server_msg)).await;
                                }
                            }
                            Ok(None) => {
                                tracing::info!(
                                    "gRPC client closed stream: {}",
                                    conn_id_clone
                                );
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "gRPC inbound error for {}: {}",
                                    conn_id_clone,
                                    e
                                );
                                break;
                            }
                        }
                    }

                    // Branch 2: Server-push message via IPC session channel
                    // (IntentReceived, LLMConfigDelivery, WorkspaceContextUpdate, etc.)
                    push_msg = ipc_push_rx.recv() => {
                        match push_msg {
                            Some(msg) => {
                                tracing::debug!(
                                    "Server-push to gRPC {}: {:?}",
                                    conn_id_clone,
                                    std::mem::discriminant(&msg)
                                );
                                let server_msg = msg.to_proto(0); // request_id = 0 = push
                                if outbound_tx.send(Ok(server_msg)).await.is_err() {
                                    tracing::warn!(
                                        "gRPC outbound channel closed for {}",
                                        conn_id_clone
                                    );
                                    break;
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "IPC push channel closed for {}",
                                    conn_id_clone
                                );
                                break;
                            }
                        }
                    }

                    // Branch 3: CapabilityUpdate broadcast
                    cap_msg = cap_rx.recv() => {
                        match cap_msg {
                            Ok(msg) => {
                                tracing::debug!(
                                    "CapabilityUpdate broadcast to gRPC {}",
                                    conn_id_clone
                                );
                                let server_msg = msg.to_proto(0);
                                if outbound_tx.send(Ok(server_msg)).await.is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(
                                    "CapabilityUpdate channel lagged for {}: skipped {} messages",
                                    conn_id_clone, n
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::info!(
                                    "CapabilityUpdate channel closed for {}",
                                    conn_id_clone
                                );
                            }
                        }
                    }
                }
            }

            // Cleanup on disconnect
            let removed_session = {
                let mut mgr = grpc_session_mgr.lock().await;
                mgr.remove_session(&conn_id_clone)
            };
            {
                let mut mgr = ipc_session_mgr.lock().await;
                mgr.remove_session(&conn_id_clone);
            }
            if let Some(session) = removed_session
                && let Some(agent_id) = session.agent_id
            {
                let mut gw = state.write().await;
                gw.set_agent_connected(&agent_id, false);
                tracing::info!(
                    "Agent {} disconnected (conn={}), connected set to false",
                    agent_id, conn_id_clone
                );
            }
            tracing::info!("gRPC connection {} cleaned up", conn_id_clone);
        });

        let output_stream =
            tokio_stream::wrappers::ReceiverStream::new(outbound_rx);
        Ok(Response::new(output_stream))
    }
}

/// Atomic counter for gRPC connection IDs
static CONN_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Server startup ──────────────────────────────────────────────────────────

/// Start the gRPC server on the given address.
///
/// This creates a `GatewayGrpcService` from the provided shared state
/// and serves it via tonic's `Server`. The server listens on
/// `127.0.0.1:19877` by default.
pub async fn start_grpc_server(
    addr: SocketAddr,
    state: SharedState,
    grpc_session_mgr: SharedGrpcSessionMgr,
    ipc_session_mgr: SharedSessionMgr,
    capability_tx: tokio::sync::broadcast::Sender<GatewayResponse>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: Option<SessionPendingRequests>,
    approval_pending: Option<ApprovalPendingRequests>,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = GatewayGrpcService {
        state,
        grpc_session_mgr,
        ipc_session_mgr,
        capability_tx,
        bridge_tx,
        session_pending,
        approval_pending,
    };

    tracing::info!("gRPC server starting on {}", addr);

    tonic::transport::Server::builder()
        .add_service(GatewayServiceServer::new(service))
        .serve(addr)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?;

    Ok(())
}

/// Build a default gRPC listen address.
pub fn default_grpc_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], DEFAULT_GRPC_PORT))
}

/// Check if a ClientMessage is a memory API response from Runtime.
/// These are routed to the pending request map, not through dispatch.
fn is_memory_result(msg: &proto::ClientMessage) -> bool {
    matches!(
        msg.payload,
        Some(proto::client_message::Payload::MemoryNodesResult(_))
            | Some(proto::client_message::Payload::MemoryStatsResult(_))
            | Some(proto::client_message::Payload::MemoryConsolidateResult(_))
            | Some(proto::client_message::Payload::MemoryDeleteResult(_))
    )
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_session_new() {
        let (tx, _rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        let session = GrpcSession::new(tx);
        assert!(session.agent_id.is_none());
        assert!(!session.authenticated);
    }

    #[test]
    fn test_grpc_session_authenticate() {
        let (tx, _rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        let mut session = GrpcSession::new(tx);
        session.authenticate("com.example.test");
        assert_eq!(session.agent_id, Some("com.example.test".to_string()));
        assert!(session.authenticated);
    }

    #[test]
    fn test_grpc_session_manager_create() {
        let mut mgr = GrpcSessionManager::new();
        let (tx, _rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        mgr.create_session("grpc-1", tx);
        assert!(mgr.get_session("grpc-1").is_some());
    }

    #[test]
    fn test_grpc_session_manager_remove() {
        let mut mgr = GrpcSessionManager::new();
        let (tx, _rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        mgr.create_session("grpc-1", tx);
        mgr.remove_session("grpc-1");
        assert!(mgr.get_session("grpc-1").is_none());
    }

    #[test]
    fn test_grpc_session_manager_find_by_agent_id() {
        let mut mgr = GrpcSessionManager::new();
        let (tx, _rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        mgr.create_session("grpc-1", tx);
        mgr.get_session_mut("grpc-1").unwrap().authenticate("com.example.weather");

        let result = mgr.find_by_agent_id("com.example.weather");
        assert!(result.is_some());

        let not_found = mgr.find_by_agent_id("com.example.unknown");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_default_grpc_addr() {
        let addr = default_grpc_addr();
        assert_eq!(addr.port(), 19877);
        assert!(addr.is_ipv4());
    }

    #[tokio::test]
    async fn test_grpc_session_push_message() {
        let (tx, mut rx) = mpsc::channel::<Result<proto::ServerMessage, Status>>(8);
        let session = GrpcSession::new(tx);

        let response = GatewayResponse::BudgetInfo {
            remaining_tokens: 1000,
            remaining_cost_usd: 5.0,
        };

        let pushed = session.push_message(response).await;
        assert!(pushed);

        let msg = rx.try_recv().expect("Should have received a message");
        assert!(msg.is_ok());
        let server_msg = msg.unwrap();
        assert_eq!(server_msg.request_id, 0); // push messages use request_id = 0
    }
}
