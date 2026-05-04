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

use tokio::sync::{Mutex, mpsc};
use tonic::{Request, Response, Status, Streaming};

use rollball_core::proto;
use rollball_core::proto::gateway_service_server::{GatewayService, GatewayServiceServer};
use rollball_core::proto_bridge::GatewayResponseToProto;
use rollball_core::protocol::GatewayResponse;

use crate::ipc::server::{SharedPermissionStore, SharedState};
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
}

// ── GrpcSessionManager ──────────────────────────────────────────────────────

/// Manages all active gRPC sessions.
pub struct GrpcSessionManager {
    sessions: HashMap<String, GrpcSession>,
}

impl GrpcSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
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

    /// Remove a session (on disconnect)
    pub fn remove_session(&mut self, conn_id: &str) -> Option<GrpcSession> {
        self.sessions.remove(conn_id)
    }

    /// Find session by agent_id (only main connections)
    pub fn find_by_agent_id(&self, agent_id: &str) -> Option<(&String, &GrpcSession)> {
        self.sessions.iter().find(|(_, s)| {
            s.agent_id.as_deref() == Some(agent_id) && s.connection_role == "main"
        })
    }
}

impl Default for GrpcSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared gRPC session manager type
type SharedGrpcSessionMgr = Arc<Mutex<GrpcSessionManager>>;

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
    perm_store: SharedPermissionStore,
    capability_tx: tokio::sync::broadcast::Sender<GatewayResponse>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: Option<SessionPendingRequests>,
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
        let perm_store = Arc::clone(&self.perm_store);
        let mut cap_rx = self.capability_tx.subscribe();
        let bridge_tx = self.bridge_tx.clone();
        let session_pending = self.session_pending.clone();
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

                                // Stream chunks: dispatch but don't send response
                                if is_stream_chunk(&client_msg) {
                                    // For stream chunks, we still dispatch for bridge forwarding
                                    let _ = dispatch_grpc_request(
                                        client_msg,
                                        &conn_id_clone,
                                        &state,
                                        &ipc_session_mgr,
                                        &perm_store,
                                        &bridge_tx,
                                        &session_pending,
                                    ).await;
                                    continue;
                                }

                                // Dispatch and send response
                                let server_msg = dispatch_grpc_request(
                                    client_msg,
                                    &conn_id_clone,
                                    &state,
                                    &ipc_session_mgr,
                                    &perm_store,
                                    &bridge_tx,
                                    &session_pending,
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
    ipc_session_mgr: SharedSessionMgr,
    perm_store: SharedPermissionStore,
    capability_tx: tokio::sync::broadcast::Sender<GatewayResponse>,
    bridge_tx: Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: Option<SessionPendingRequests>,
) -> Result<(), Box<dyn std::error::Error>> {
    let grpc_session_mgr = Arc::new(Mutex::new(GrpcSessionManager::new()));

    let service = GatewayGrpcService {
        state,
        grpc_session_mgr,
        ipc_session_mgr,
        perm_store,
        capability_tx,
        bridge_tx,
        session_pending,
    };

    tracing::info!("gRPC server starting on {}", addr);

    tonic::transport::Server::builder()
        .add_service(GatewayServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

/// Build a default gRPC listen address.
pub fn default_grpc_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], DEFAULT_GRPC_PORT))
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
