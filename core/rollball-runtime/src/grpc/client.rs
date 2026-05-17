//! gRPC-based Gateway client using bidirectional streaming.
//!
//! Replaces the legacy socket-based `GatewayClient` with a protocol-buffer
//! transport that natively supports multiplexing: request-response and
//! server-push messages share a single gRPC stream and are demuxed by
//! `request_id`. This eliminates the IPC frame interleaving bug that
//! required `pending_push` buffering in the old client.
//!
//! Key improvements over the legacy IPC client:
//! - **No frame interleaving**: gRPC stream multiplexes inherently
//! - **Concurrent requests**: each gets a unique `request_id`; `&self` sends
//! - **Exponential backoff reconnect**: [`connect`] wraps [`connect_once`] with configurable bounds
//! - **Protocol buffer types**: strongly-typed messages replace ad-hoc JSON frames

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use rollball_core::budget;
use rollball_core::error::RollballError;
use rollball_core::proto;
use rollball_core::proto::server_message::Payload as ServerPayload;
use rollball_core::proto_bridge::GatewayRequestToProto;
use rollball_core::protocol::{GatewayRequest, GatewayResponse, ModelCapabilitiesInfo, ProtocolType};

/// Configuration delivered by Gateway in the AgentHelloResult handshake.
///
/// Bundles LLM config, workspace context, and runtime overrides into a
/// single atomic response so the Runtime does not need to selectively
/// read from the shared push channel during startup.
#[derive(Debug, Clone)]
pub struct AgentHelloConfig {
    // ── LLM Configuration ──
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<String>,
    pub model_capabilities: Option<ModelCapabilitiesInfo>,
    pub max_output_tokens_limit: u64,
    pub protocol_type: ProtocolType,

    // ── Workspace Context ──
    pub workspace_context_text: Option<String>,
    pub current_workspace_id: Option<String>,
    pub current_workspace_path: Option<String>,

    // ── Runtime Config Overrides ──
    pub runtime_max_output_tokens: Option<u64>,
    pub runtime_max_iterations: Option<u32>,
    pub runtime_temperature: Option<f32>,
    pub runtime_system_prompt_override: Option<String>,
}

/// Request timeout for individual RPC calls
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of pending usage reports to buffer (S4.5.2)
const MAX_PENDING_REPORTS: usize = 100;

// ── GatewayGrpcClient ─────────────────────────────────────────────────────

/// gRPC-based Gateway client using bidirectional streaming.
///
/// See module-level documentation for design rationale.
pub struct GatewayGrpcClient {
    /// The endpoint URI (retained for reconnect)
    endpoint: String,
    /// Outbound message sender (feeds the gRPC stream via ReceiverStream)
    outbound_tx: mpsc::Sender<proto::ClientMessage>,
    /// Request ID counter (atomic for concurrent access from `&self` methods)
    next_request_id: AtomicU64,
    /// Pending request map: request_id → oneshot sender for response
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<proto::ServerMessage>>>>,
    /// Push message receiver (consumed by `recv_message()`)
    push_rx: mpsc::UnboundedReceiver<proto::ServerMessage>,
    /// Connection status flag (set to false by inbound task on stream close)
    connected: Arc<AtomicBool>,
    /// S4.5.2: Pending usage reports buffered during disconnect
    pending_reports: Arc<Mutex<VecDeque<budget::UsageReport>>>,
    /// Memory query receiver — inbound loop forwards MemoryXxxQuery here.
    /// The agent loop polls this channel and handles memory operations.
    /// Wrapped in Option to allow take_memory_query_rx() extraction
    /// for tokio::select! without &mut self conflicts.
    memory_query_rx: Option<mpsc::UnboundedReceiver<(u64, proto::server_message::Payload)>>,
}

impl GatewayGrpcClient {
    // ── Connection ─────────────────────────────────────────────────────────

    /// Connect to Gateway gRPC endpoint (single attempt, no retry).
    ///
    /// Creates a tonic `Channel`, instantiates the `GatewayServiceClient`,
    /// and opens the `Connect` bidi-stream RPC. An inbound receive loop is
    /// spawned as a background task that demuxes responses and push messages.
    ///
    /// Prefer [`connect`] for production use — it wraps this with exponential
    /// backoff retry.
    async fn connect_once(endpoint: &str) -> Result<Self, RollballError> {
        let channel = Channel::from_shared(endpoint.to_string())
            .map_err(|e| RollballError::Ipc(format!("Invalid gRPC endpoint: {}", e)))?
            .connect()
            .await
            .map_err(|e| RollballError::Ipc(format!("gRPC connection failed: {}", e)))?;

        let mut client =
            proto::gateway_service_client::GatewayServiceClient::new(channel);

        // Outbound channel: Runtime → Gateway
        let (outbound_tx, outbound_rx) = mpsc::channel::<proto::ClientMessage>(256);
        let outbound_stream = ReceiverStream::new(outbound_rx);

        // Open bidi-stream RPC
        let response = client
            .connect(outbound_stream)
            .await
            .map_err(|e| RollballError::Ipc(format!("gRPC stream establishment failed: {}", e)))?;
        let mut inbound = response.into_inner();

        // Internal state
        let pending = Arc::new(Mutex::new(
            HashMap::<u64, oneshot::Sender<proto::ServerMessage>>::new(),
        ));
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let (memory_query_tx, memory_query_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(true));


        // Spawn inbound receive loop.
        // When this task exits, push_tx is dropped, causing push_rx.recv() to
        // eventually return None — which signals connection loss to recv_message().
        let pending_clone = Arc::clone(&pending);
        let connected_clone = Arc::clone(&connected);
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(msg)) => {
                        // Check if this is a memory API query from Gateway.
                        // These require non-trivial request_id for Gateway→Runtime
                        // request-response, and are forwarded to the agent loop.
                        if is_memory_query_payload(&msg) {
                            if let Some(payload) = msg.payload {
                                let _ = memory_query_tx.send((msg.request_id, payload));
                            }
                            continue;
                        }

                        if msg.request_id == 0 {
                            // Server-push message → forward to push_rx
                            if push_tx.send(msg).is_err() {
                                tracing::warn!("Push channel closed, inbound loop exiting");
                                break;
                            }
                        } else {
                            // Response → fulfill pending oneshot
                            let mut map = pending_clone.lock().await;
                            if let Some(sender) = map.remove(&msg.request_id) {
                                let _ = sender.send(msg);
                            } else {
                                tracing::warn!(
                                    request_id = msg.request_id,
                                    "Received response for unknown request_id"
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!("gRPC stream closed by server");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "gRPC stream error");
                        break;
                    }
                }
            }
            // Signal disconnection
            connected_clone.store(false, Ordering::SeqCst);
        });

        tracing::info!(endpoint = %endpoint, "Gateway gRPC client connected");

        Ok(Self {
            endpoint: endpoint.to_string(),
            outbound_tx,
            next_request_id: AtomicU64::new(1),
            pending,
            push_rx,
            connected,
            pending_reports: Arc::new(Mutex::new(VecDeque::new())),
            memory_query_rx: Some(memory_query_rx),
        })
    }

    /// Connect to Gateway gRPC with exponential backoff retry.
    ///
    /// Initial delay 100 ms, max delay 30 s, total timeout controlled by
    /// `max_elapsed_secs` (defaults to 300 s for production).
    /// This is the primary connection method for production use.
    pub async fn connect(endpoint: &str) -> Result<Self, RollballError> {
        Self::connect_with_timeout(endpoint, 300).await
    }

    /// Connect with a custom max elapsed time (useful for tests).
    pub async fn connect_with_timeout(
        endpoint: &str,
        max_elapsed_secs: u64,
    ) -> Result<Self, RollballError> {
        const INITIAL_DELAY_MS: u64 = 100;
        const MAX_DELAY_MS: u64 = 30_000;

        let start = std::time::Instant::now();
        let mut delay_ms = INITIAL_DELAY_MS;

        loop {
            match Self::connect_once(endpoint).await {
                Ok(client) => {
                    tracing::info!(
                        endpoint = %endpoint,
                        "Connected to Gateway gRPC with retry"
                    );
                    return Ok(client);
                }
                Err(e) => {
                    let elapsed = start.elapsed().as_secs();
                    if elapsed >= max_elapsed_secs {
                        return Err(RollballError::Ipc(format!(
                            "Failed to connect to Gateway gRPC after {}s: {}",
                            max_elapsed_secs, e
                        )));
                    }
                    tracing::warn!(
                        delay_ms,
                        elapsed_s = elapsed,
                        error = %e,
                        "gRPC connection failed, retrying in {}ms",
                        delay_ms
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = std::cmp::min(delay_ms * 2, MAX_DELAY_MS);
                }
            }
        }
    }

    /// Convenience: connect as "main" role and send AgentHello.
    pub async fn connect_and_register(
        endpoint: &str,
        agent_id: &str,
        version: &str,
    ) -> Result<(Self, AgentHelloConfig), RollballError> {
        Self::connect_and_register_with_role(endpoint, agent_id, version, "main").await
    }

    /// Convenience: connect with a specific connection role and send AgentHello.
    ///
    /// Returns the client and the bundled [`AgentHelloConfig`] from the handshake.
    pub async fn connect_and_register_with_role(
        endpoint: &str,
        agent_id: &str,
        version: &str,
        connection_role: &str,
    ) -> Result<(Self, AgentHelloConfig), RollballError> {
        let client = Self::connect(endpoint).await?;
        let config = client
            .send_agent_hello(agent_id, version, connection_role)
            .await?;
        Ok((client, config))
    }

    /// Reconnect with exponential backoff and re-register with Gateway.
    ///
    /// Preserves buffered usage reports and replays them after re-registering.
    pub async fn reconnect_and_reregister(
        &mut self,
        agent_id: &str,
        version: &str,
    ) -> Result<(), RollballError> {
        // Save pending reports before replacing self
        let saved_reports = {
            let mut guard = self.pending_reports.lock().await;
            std::mem::take(&mut *guard)
        };

        *self = Self::connect(&self.endpoint).await?;

        // Restore pending reports
        {
            let mut guard = self.pending_reports.lock().await;
            *guard = saved_reports;
        }

        let _config = self.send_agent_hello(agent_id, version, "main").await?;
        self.flush_pending_reports().await?;
        Ok(())
    }

    /// Get a clone of the outbound message sender.
    ///
    /// Allows external tasks (e.g. chunk relay) to send messages through
    /// the shared gRPC stream without needing a full `GatewayGrpcClient`.
    pub fn outbound_sender(&self) -> mpsc::Sender<proto::ClientMessage> {
        self.outbound_tx.clone()
    }

    /// Take the memory query receiver out of the client.
    ///
    /// This is needed so the agent loop can `tokio::select!` on both
    /// `recv_message()` and the memory query channel without &mut self
    /// conflicts. Returns None if already taken.
    pub fn take_memory_query_rx(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<(u64, proto::server_message::Payload)>> {
        self.memory_query_rx.take()
    }

    // ── Status ─────────────────────────────────────────────────────────────

    /// Check if the client is connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Get the endpoint URI this client is connected to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    // ── Core send/receive ──────────────────────────────────────────────────

    /// Send a domain `GatewayRequest` and wait for the corresponding response.
    ///
    /// Assigns a unique `request_id`, inserts a oneshot into the pending map,
    /// sends the message, and waits with a 30-second timeout.
    async fn send_gateway_request(
        &self,
        request: GatewayRequest,
    ) -> Result<proto::ServerMessage, RollballError> {
        if !self.is_connected() {
            return Err(RollballError::Ipc("Not connected to Gateway".to_string()));
        }

        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();

        {
            let mut map = self.pending.lock().await;
            map.insert(request_id, tx);
        }

        let msg = request.to_proto(request_id);
        self.outbound_tx
            .send(msg)
            .await
            .map_err(|e| RollballError::Ipc(format!("Failed to send request: {}", e)))?;

        tokio::time::timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| {
                // Clean up pending entry on timeout
                let pending = Arc::clone(&self.pending);
                tokio::spawn(async move {
                    let mut map = pending.lock().await;
                    map.remove(&request_id);
                });
                RollballError::Ipc(format!(
                    "Request {} timed out after {:?}",
                    request_id, REQUEST_TIMEOUT
                ))
            })?
            .map_err(|_| {
                RollballError::Ipc(format!(
                    "Response channel closed for request {}",
                    request_id
                ))
            })
    }

    /// Receive a server-push message as a domain `GatewayResponse`.
    ///
    /// Blocks until a push message arrives or the stream closes.
    /// Returns `Ok(None)` when the connection is closed (matching the
    /// legacy `GatewayClient::recv_message()` API).
    pub async fn recv_message(&mut self) -> Result<Option<GatewayResponse>, RollballError> {
        match self.push_rx.recv().await {
            Some(msg) => {
                let response = proto_to_gateway_response(msg);
                Ok(Some(response))
            }
            None => {
                // All push senders dropped — inbound loop exited
                tracing::info!("gRPC push channel closed (stream ended)");
                Ok(None)
            }
        }
    }

    // ── High-level API (matching old GatewayClient surface) ────────────────

    /// Send AgentHello to register with the Gateway.
    ///
    /// Returns the bundled [`AgentHelloConfig`] containing LLM configuration,
    /// workspace context, and runtime overrides — all delivered atomically
    /// in the AgentHelloResult response (no separate push messages needed).
    pub async fn send_agent_hello(
        &self,
        agent_id: &str,
        version: &str,
        connection_role: &str,
    ) -> Result<AgentHelloConfig, RollballError> {
        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::AgentHelloResult(result)) => {
                if result.success {
                    if !result.error.is_empty() {
                        tracing::warn!(
                            "AgentHello succeeded but with error: {}",
                            result.error
                        );
                    }
                    tracing::info!(agent_id = %agent_id, "Gateway registered agent via gRPC");

                    let config = AgentHelloConfig {
                        provider: if result.provider.is_empty() {
                            None
                        } else {
                            Some(result.provider)
                        },
                        model: if result.model.is_empty() {
                            None
                        } else {
                            Some(result.model)
                        },
                        api_key: if result.api_key.is_empty() {
                            None
                        } else {
                            Some(result.api_key)
                        },
                        base_url: if result.base_url.is_empty() {
                            None
                        } else {
                            Some(result.base_url)
                        },
                        models: result.models,
                        model_capabilities: result.model_capabilities.map(|c| c.into()),
                        max_output_tokens_limit: result.max_output_tokens_limit,
                        protocol_type: match result.protocol_type.as_str() {
                            "anthropic" => ProtocolType::Anthropic,
                            "ollama" => ProtocolType::Ollama,
                            _ => ProtocolType::OpenAI,
                        },
                        workspace_context_text: if result.workspace_context_text.is_empty() {
                            None
                        } else {
                            Some(result.workspace_context_text)
                        },
                        current_workspace_id: if result.current_workspace_id.is_empty() {
                            None
                        } else {
                            Some(result.current_workspace_id)
                        },
                        current_workspace_path: if result.current_workspace_path.is_empty() {
                            None
                        } else {
                            Some(result.current_workspace_path)
                        },
                        runtime_max_output_tokens: result.runtime_max_output_tokens,
                        runtime_max_iterations: result.runtime_max_iterations,
                        runtime_temperature: result.runtime_temperature,
                        runtime_system_prompt_override: if result
                            .runtime_system_prompt_override
                            .is_empty()
                        {
                            None
                        } else {
                            Some(result.runtime_system_prompt_override)
                        },
                    };
                    Ok(config)
                } else {
                    Err(RollballError::Ipc(format!(
                        "AgentHello rejected: {}",
                        result.error
                    )))
                }
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected AgentHello response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty AgentHello response".to_string(),
            )),
        }
    }

    /// Query remaining budget for a provider.
    pub async fn query_budget(&self, provider: &str) -> Result<(u64, f64), RollballError> {
        let request = GatewayRequest::BudgetQuery {
            provider: provider.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::BudgetInfo(info)) => {
                Ok((info.remaining_tokens, info.remaining_cost_usd))
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Send an Intent to another Agent.
    pub async fn send_intent(
        &self,
        target: &str,
        action: &str,
        params: serde_json::Value,
        async_: bool,
    ) -> Result<String, RollballError> {
        let request = GatewayRequest::IntentSend {
            target: target.to_string(),
            action: action.to_string(),
            params,
            async_,
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::IntentDelivered(delivered)) => Ok(delivered.message_id),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Send a streaming chunk (fire-and-forget, no response expected).
    ///
    /// Uses `request_id: 0` to indicate no correlation is needed.
    /// The Gateway broadcasts the chunk without generating a response.
    pub async fn send_stream_chunk(
        &self,
        target: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<(), RollballError> {
        let msg = proto::ClientMessage {
            request_id: 0,
            payload: Some(proto::client_message::Payload::StreamChunk(
                proto::StreamChunk {
                    target: target.to_string(),
                    action: action.to_string(),
                    params_json: params.to_string(),
                },
            )),
        };
        self.outbound_tx
            .send(msg)
            .await
            .map_err(|e| RollballError::Ipc(format!("Failed to send stream chunk: {}", e)))?;
        Ok(())
    }

    /// Request an API key for a specific provider (KeyRelease).
    pub async fn request_key(&self, provider: &str) -> Result<String, RollballError> {
        let request = GatewayRequest::KeyRelease {
            provider: provider.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::KeyReleaseResult(result)) => {
                if !result.api_key.is_empty() && result.error.is_empty() {
                    Ok(result.api_key)
                } else if !result.error.is_empty() {
                    Err(RollballError::Ipc(result.error))
                } else {
                    Err(RollballError::Ipc(
                        "KeyRelease returned no key and no error".to_string(),
                    ))
                }
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Report token usage to Gateway.
    ///
    /// S4.5.2: If disconnected, the report is buffered and
    /// will be sent on reconnect via `flush_pending_reports`.
    pub async fn report_usage(
        &self,
        report: budget::UsageReport,
    ) -> Result<(), RollballError> {
        if !self.is_connected() {
            // S4.5.2: Buffer for later delivery
            let mut guard = self.pending_reports.lock().await;
            if guard.len() >= MAX_PENDING_REPORTS {
                guard.pop_front(); // Drop oldest to make room
            }
            guard.push_back(report);
            tracing::debug!(
                "Buffered usage report (disconnected), pending={}",
                guard.len()
            );
            return Ok(());
        }

        let request = GatewayRequest::UsageReport(report);
        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::UsageReportAck(_)) => Ok(()),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// S4.5.2: Flush pending usage reports after reconnect.
    ///
    /// Sends all buffered reports to the Gateway. Reports that fail
    /// to send are re-buffered for the next attempt.
    pub async fn flush_pending_reports(&self) -> Result<usize, RollballError> {
        let reports: Vec<budget::UsageReport> = {
            let mut guard = self.pending_reports.lock().await;
            std::mem::take(&mut *guard).into_iter().collect()
        };

        let mut sent = 0;
        for report in &reports {
            match self.report_usage(report.clone()).await {
                Ok(()) => sent += 1,
                Err(_) => {
                    // Re-buffer failed reports
                    let mut guard = self.pending_reports.lock().await;
                    if guard.len() < MAX_PENDING_REPORTS {
                        guard.push_back(report.clone());
                    }
                }
            }
        }

        Ok(sent)
    }

    /// Get the number of pending usage reports.
    pub async fn pending_report_count(&self) -> usize {
        self.pending_reports.lock().await.len()
    }

    /// Acquire a rate limit token.
    pub async fn acquire_rate_token(
        &self,
        provider: &str,
    ) -> Result<(bool, Option<u64>), RollballError> {
        let request = GatewayRequest::RateAcquire {
            provider: provider.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::RateToken(token)) => Ok((
                token.granted,
                if token.retry_after_ms == 0 {
                    None
                } else {
                    Some(token.retry_after_ms)
                },
            )),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Request a runtime permission.
    pub async fn request_permission(
        &self,
        permission: &str,
        reason: &str,
    ) -> Result<(bool, Option<String>), RollballError> {
        self.request_permission_with_timeout(
            permission,
            reason,
            rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
        )
        .await
    }

    /// Request a runtime permission with a custom timeout.
    pub async fn request_permission_with_timeout(
        &self,
        permission: &str,
        reason: &str,
        timeout_ms: u64,
    ) -> Result<(bool, Option<String>), RollballError> {
        let perm_req_id = format!(
            "perm-{}-{}",
            self.next_request_id.fetch_add(1, Ordering::SeqCst),
            chrono::Utc::now().timestamp_millis()
        );

        let request = GatewayRequest::PermissionRequest {
            request_id: perm_req_id.clone(),
            permission: permission.to_string(),
            reason: reason.to_string(),
            timeout_ms,
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::PermissionResult(result)) => {
                if result.request_id != perm_req_id {
                    tracing::warn!(
                        expected = %perm_req_id,
                        got = %result.request_id,
                        "PermissionResult request_id mismatch"
                    );
                }
                Ok((
                    result.granted,
                    if result.reason.is_empty() {
                        None
                    } else {
                        Some(result.reason)
                    },
                ))
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Report context usage to Gateway after each LLM call.
    pub async fn report_context_usage(
        &self,
        agent_id: &str,
        context: rollball_core::protocol::ContextUsageInfo,
    ) -> Result<(), RollballError> {
        let request = GatewayRequest::ContextUsageReport {
            agent_id: agent_id.to_string(),
            context,
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::ContextUsageAck(_)) => Ok(()),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Query capabilities from the Gateway.
    pub async fn query_capabilities(
        &self,
        agent_id: Option<&str>,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, RollballError> {
        let request = GatewayRequest::CapabilityQuery {
            agent_id: agent_id.map(|s| s.to_string()),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::CapabilityOverview(overview)) => {
                Ok(overview
                    .capabilities
                    .into_iter()
                    .map(|(k, v)| (k, v.items))
                    .collect())
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Query identity fields from the Gateway.
    pub async fn query_identity(
        &self,
        fields: &[String],
    ) -> Result<rollball_core::identity::IdentityQueryResult, RollballError> {
        let request = GatewayRequest::IdentityQuery {
            fields: fields.to_vec(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::IdentityQueryResult(result)) => Ok(result.into()),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Register a cron job with the Gateway.
    ///
    /// Returns `Ok(cron_id)` on success, `Err(error_message)` on failure.
    pub async fn register_cron(
        &self,
        agent_id: &str,
        schedule: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<Result<String, String>, RollballError> {
        let request = GatewayRequest::CronRegister {
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params,
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::CronRegisterResult(result)) => {
                if result.error.is_empty() {
                    Ok(Ok(result.cron_id))
                } else {
                    Ok(Err(result.error))
                }
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Unregister a cron job.
    pub async fn unregister_cron(&self, cron_id: &str) -> Result<bool, RollballError> {
        let request = GatewayRequest::CronUnregister {
            cron_id: cron_id.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::CronUnregisterResult(result)) => Ok(result.removed),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// List registered cron jobs.
    pub async fn list_cron(
        &self,
    ) -> Result<Vec<rollball_core::protocol::CronEntryInfo>, RollballError> {
        let request = GatewayRequest::CronList {};

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::CronListResult(result)) => {
                Ok(result.entries.into_iter().map(|e| e.into()).collect())
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// List conversation sessions.
    pub async fn list_sessions(
        &self,
    ) -> Result<Vec<rollball_core::protocol::SessionInfoDto>, RollballError> {
        let request = GatewayRequest::ListSessions;

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::SessionList(list)) => {
                Ok(list.sessions.into_iter().map(|s| s.into()).collect())
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Get paginated messages from a session.
    pub async fn get_session_messages(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: u32,
        direction: &str,
    ) -> Result<
        (
            Vec<rollball_core::protocol::ConversationEntryDto>,
            Option<String>,
            bool,
        ),
        RollballError,
    > {
        let request = GatewayRequest::GetSessionMessages {
            session_id: session_id.to_string(),
            cursor: cursor.map(|s| s.to_string()),
            limit,
            direction: direction.to_string(),
        };

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::SessionMessages(sm)) => {
                let messages = sm.messages.into_iter().map(|m| m.into()).collect();
                let cursor = if sm.cursor.is_empty() {
                    None
                } else {
                    Some(sm.cursor)
                };
                Ok((messages, cursor, sm.has_more))
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Create a new conversation session.
    pub async fn create_session(&self) -> Result<String, RollballError> {
        let request = GatewayRequest::CreateSession;

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::SessionCreated(sc)) => Ok(sc.session_id),
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Get the current session ID.
    pub async fn get_current_session_id(&self) -> Result<Option<String>, RollballError> {
        let request = GatewayRequest::GetCurrentSessionId;

        let resp = self.send_gateway_request(request).await?;
        match resp.payload {
            Some(ServerPayload::CurrentSessionId(csid)) => {
                Ok(if csid.session_id.is_empty() {
                    None
                } else {
                    Some(csid.session_id)
                })
            }
            Some(other) => Err(RollballError::Ipc(format!(
                "Unexpected response: {:?}",
                other
            ))),
            None => Err(RollballError::Ipc(
                "Empty response payload".to_string(),
            )),
        }
    }
}

// ── Proto → Domain conversion ─────────────────────────────────────────────

/// Convert a proto `ServerMessage` to a domain `GatewayResponse`.
///
/// Handles all payload variants. Empty strings are converted to `None` for
/// optional fields, matching the proto→domain convention in `proto_bridge.rs`.
fn proto_to_gateway_response(msg: proto::ServerMessage) -> GatewayResponse {
    match msg.payload {
        // Push messages (request_id == 0)
        Some(ServerPayload::IntentReceived(ir)) => {
            let params: serde_json::Value = serde_json::from_str(&ir.params_json)
                .unwrap_or(serde_json::Value::Null);
            GatewayResponse::IntentReceived {
                from: ir.from,
                action: ir.action,
                params,
                command: if ir.command.is_empty() { None } else { Some(ir.command) },
            }
        }
        Some(ServerPayload::CapabilityUpdate(cu)) => GatewayResponse::CapabilityUpdate {
            agent_id: cu.agent_id,
            actions: cu.actions,
            removed: cu.removed,
        },
        Some(ServerPayload::LlmConfigDelivery(cfg)) => GatewayResponse::LLMConfigDelivery {
            provider: cfg.provider,
            model: if cfg.model.is_empty() {
                None
            } else {
                Some(cfg.model)
            },
            api_key: cfg.api_key,
            base_url: if cfg.base_url.is_empty() {
                None
            } else {
                Some(cfg.base_url)
            },
            models: cfg.models,
            model_capabilities: cfg.model_capabilities.map(|c| c.into()),
            max_output_tokens_limit: cfg.max_output_tokens_limit,
            protocol_type: match cfg.protocol_type.as_str() {
                "anthropic" => ProtocolType::Anthropic,
                "ollama" => ProtocolType::Ollama,
                _ => ProtocolType::OpenAI,
            },
        },
        Some(ServerPayload::IdentityDelivery(id)) => GatewayResponse::IdentityDelivery {
            entries: id.entries.into_iter().map(|e| e.into()).collect(),
        },
        Some(ServerPayload::WorkspaceContextUpdate(wcu)) => {
            GatewayResponse::WorkspaceContextUpdate {
                context_text: wcu.context_text,
                current_workspace_id: if wcu.current_workspace_id.is_empty() {
                    None
                } else {
                    Some(wcu.current_workspace_id)
                },
                current_workspace_path: if wcu.current_workspace_path.is_empty() {
                    None
                } else {
                    Some(wcu.current_workspace_path)
                },
            }
        }
        Some(ServerPayload::IterationLimitPaused(ilp)) => {
            GatewayResponse::IterationLimitPaused {
                iteration: ilp.iteration,
                max_iterations: ilp.max_iterations,
                message: ilp.message,
            }
        }
        Some(ServerPayload::RuntimeConfigUpdate(rcu)) => {
            GatewayResponse::RuntimeConfigUpdate {
                max_output_tokens: rcu.max_output_tokens,
                max_iterations: rcu.max_iterations,
                temperature: rcu.temperature,
                system_prompt_override: if rcu.system_prompt_override.is_empty() {
                    None
                } else {
                    Some(rcu.system_prompt_override)
                },
            }
        }

        // Response messages (request_id > 0) — included for robustness
        Some(ServerPayload::AgentHelloResult(r)) => GatewayResponse::AgentHelloResult {
            success: r.success,
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
            provider: if r.provider.is_empty() { None } else { Some(r.provider) },
            model: if r.model.is_empty() { None } else { Some(r.model) },
            api_key: if r.api_key.is_empty() { None } else { Some(r.api_key) },
            base_url: if r.base_url.is_empty() { None } else { Some(r.base_url) },
            models: r.models,
            model_capabilities: r.model_capabilities.map(|c| c.into()),
            max_output_tokens_limit: r.max_output_tokens_limit,
            protocol_type: match r.protocol_type.as_str() {
                "anthropic" => ProtocolType::Anthropic,
                "ollama" => ProtocolType::Ollama,
                _ => ProtocolType::OpenAI,
            },
            workspace_context_text: if r.workspace_context_text.is_empty() {
                None
            } else {
                Some(r.workspace_context_text)
            },
            current_workspace_id: if r.current_workspace_id.is_empty() {
                None
            } else {
                Some(r.current_workspace_id)
            },
            current_workspace_path: if r.current_workspace_path.is_empty() {
                None
            } else {
                Some(r.current_workspace_path)
            },
            runtime_max_output_tokens: r.runtime_max_output_tokens,
            runtime_max_iterations: r.runtime_max_iterations,
            runtime_temperature: r.runtime_temperature,
            runtime_system_prompt_override: if r.runtime_system_prompt_override.is_empty() {
                None
            } else {
                Some(r.runtime_system_prompt_override)
            },
        },
        Some(ServerPayload::KeyReleaseResult(r)) => GatewayResponse::KeyReleaseResult {
            api_key: if r.api_key.is_empty() {
                None
            } else {
                Some(r.api_key)
            },
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
        },
        Some(ServerPayload::IntentDelivered(r)) => GatewayResponse::IntentDelivered {
            message_id: r.message_id,
        },
        Some(ServerPayload::BudgetInfo(r)) => GatewayResponse::BudgetInfo {
            remaining_tokens: r.remaining_tokens,
            remaining_cost_usd: r.remaining_cost_usd,
        },
        Some(ServerPayload::UsageReportAck(_)) => GatewayResponse::UsageReportAck {},
        Some(ServerPayload::ContextUsageAck(_)) => GatewayResponse::ContextUsageAck {},
        Some(ServerPayload::RateToken(r)) => GatewayResponse::RateToken {
            granted: r.granted,
            retry_after_ms: if r.retry_after_ms == 0 {
                None
            } else {
                Some(r.retry_after_ms)
            },
        },
        Some(ServerPayload::PermissionResult(r)) => GatewayResponse::PermissionResult {
            request_id: r.request_id,
            granted: r.granted,
            reason: if r.reason.is_empty() {
                None
            } else {
                Some(r.reason)
            },
        },
        Some(ServerPayload::IdentityQueryResult(r)) => GatewayResponse::IdentityQueryResult {
            values: r.values,
            confidence: r.confidence,
        },
        Some(ServerPayload::CapabilityOverview(r)) => GatewayResponse::CapabilityOverview {
            capabilities: r
                .capabilities
                .into_iter()
                .map(|(k, v)| (k, v.items))
                .collect(),
        },
        Some(ServerPayload::CronRegisterResult(r)) => GatewayResponse::CronRegisterResult {
            cron_id: if r.cron_id.is_empty() {
                None
            } else {
                Some(r.cron_id)
            },
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
        },
        Some(ServerPayload::CronUnregisterResult(r)) => GatewayResponse::CronUnregisterResult {
            removed: r.removed,
        },
        Some(ServerPayload::CronListResult(r)) => GatewayResponse::CronListResult {
            entries: r.entries.into_iter().map(|e| e.into()).collect(),
        },
        Some(ServerPayload::SessionList(r)) => GatewayResponse::SessionList {
            sessions: r.sessions.into_iter().map(|s| s.into()).collect(),
        },
        Some(ServerPayload::SessionMessages(r)) => GatewayResponse::SessionMessages {
            messages: r.messages.into_iter().map(|m| m.into()).collect(),
            cursor: if r.cursor.is_empty() {
                None
            } else {
                Some(r.cursor)
            },
            has_more: r.has_more,
        },
        Some(ServerPayload::SessionCreated(r)) => GatewayResponse::SessionCreated {
            session_id: r.session_id,
        },
        Some(ServerPayload::CurrentSessionId(r)) => GatewayResponse::CurrentSessionId {
            session_id: if r.session_id.is_empty() {
                None
            } else {
                Some(r.session_id)
            },
        },
        Some(ServerPayload::SessionDeleted(r)) => GatewayResponse::SessionDeleted {
            success: r.success,
            error: if r.error.is_empty() {
                None
            } else {
                Some(r.error)
            },
        },
        Some(ServerPayload::LogLevelUpdate(lu)) => GatewayResponse::LogLevelUpdate {
            log_level: lu.log_level,
        },
        Some(ServerPayload::LogRotate(_)) => GatewayResponse::LogRotate,

        None => {
            tracing::warn!("Received ServerMessage with empty payload");
            GatewayResponse::Unknown {}
        }

        // Memory API query variants — handled by the agent loop via
        // dedicated GatewayResponse variants, not proto_to_gateway_response.
        _ => {
            tracing::warn!(
                "Received unrecognized ServerMessage payload variant"
            );
            GatewayResponse::Unknown {}
        }
    }
}

/// Check if a ServerMessage payload is a memory API query from Gateway.
///
/// These are gateway→runtime requests that bypass the push channel and
/// are forwarded to the agent loop via `memory_query_tx`.
fn is_memory_query_payload(msg: &proto::ServerMessage) -> bool {
    matches!(
        msg.payload,
        Some(ServerPayload::MemoryNodesQuery(_))
            | Some(ServerPayload::MemoryStatsQuery(_))
            | Some(ServerPayload::MemoryConsolidateQuery(_))
            | Some(ServerPayload::MemoryDeleteQuery(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_client_not_connected_initially() {
        // We can't construct a GatewayGrpcClient without connecting,
        // so test that the constant is correct
        assert_eq!(MAX_PENDING_REPORTS, 100);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn test_proto_to_gateway_response_budget_info() {
        let msg = proto::ServerMessage {
            request_id: 1,
            payload: Some(ServerPayload::BudgetInfo(proto::BudgetInfo {
                remaining_tokens: 50000,
                remaining_cost_usd: 1.5,
            })),
        };
        let resp = proto_to_gateway_response(msg);
        assert!(matches!(
            resp,
            GatewayResponse::BudgetInfo {
                remaining_tokens: 50000,
                remaining_cost_usd: 1.5,
            }
        ));
    }

    #[test]
    fn test_proto_to_gateway_response_intent_received() {
        let msg = proto::ServerMessage {
            request_id: 0,
            payload: Some(ServerPayload::IntentReceived(proto::IntentReceived {
                from: "com.test.agent".to_string(),
                action: "chat_message".to_string(),
                params_json: r#"{"content":"hello"}"#.to_string(),
                command: String::new(),
            })),
        };
        let resp = proto_to_gateway_response(msg);
        match resp {
            GatewayResponse::IntentReceived { from, action, params, command: _ } => {
                assert_eq!(from, "com.test.agent");
                assert_eq!(action, "chat_message");
                assert_eq!(params["content"], "hello");
            }
            _ => panic!("Expected IntentReceived"),
        }
    }

    #[test]
    fn test_proto_to_gateway_response_empty_optional_fields() {
        let msg = proto::ServerMessage {
            request_id: 0,
            payload: Some(ServerPayload::LlmConfigDelivery(
                proto::LlmConfigDelivery {
                    provider: "test".to_string(),
                    model: String::new(),   // empty → None
                    api_key: "key".to_string(),
                    base_url: String::new(), // empty → None
                    models: vec![],
                    model_capabilities: None,
                    max_output_tokens_limit: 32768,
                    protocol_type: "openai".to_string(),
                },
            )),
        };
        let resp = proto_to_gateway_response(msg);
        match resp {
            GatewayResponse::LLMConfigDelivery {
                model, base_url, ..
            } => {
                assert!(model.is_none());
                assert!(base_url.is_none());
            }
            _ => panic!("Expected LLMConfigDelivery"),
        }
    }

    #[test]
    fn test_proto_to_gateway_response_empty_payload() {
        let msg = proto::ServerMessage {
            request_id: 0,
            payload: None,
        };
        let resp = proto_to_gateway_response(msg);
        assert!(matches!(resp, GatewayResponse::Unknown {}));
    }

    #[test]
    fn test_proto_to_gateway_response_session_created() {
        let msg = proto::ServerMessage {
            request_id: 5,
            payload: Some(ServerPayload::SessionCreated(proto::SessionCreated {
                session_id: "session-123".to_string(),
            })),
        };
        let resp = proto_to_gateway_response(msg);
        assert!(matches!(
            resp,
            GatewayResponse::SessionCreated { session_id } if session_id == "session-123"
        ));
    }
}
