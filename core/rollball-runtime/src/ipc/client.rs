//! Gateway Service API client
//!
//! IPC client for communicating with the Gateway process.
//! Supports KeyRelease, IntentSend, BudgetQuery, UsageReport,
//! RateAcquire, and PermissionRequest.
//! S4.5.2: Buffers pending usage reports when disconnected,
//! and replays them on reconnect.

use std::collections::VecDeque;

use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use rollball_core::transport::AsyncTransportConnection;
use rollball_core::error::RollballError;

/// Maximum number of pending usage reports to buffer
const MAX_PENDING_REPORTS: usize = 100;

/// LLM configuration received from Gateway via IPC
///
/// Contains the user's configured provider, model, API key, and optional base URL.
/// This is the primary way Agent Runtime gets its LLM credentials (PRD GTW-05, SEC-07).
pub struct LlmConfigReceived {
    /// Provider name (e.g. "minimax", "openai")
    pub provider: String,
    /// Model identifier, or None to use manifest's suggested_model
    pub model: Option<String>,
    /// API key for the provider
    pub api_key: Option<String>,
    /// Base URL override (optional)
    pub base_url: Option<String>,
    /// Available models for this provider (user-selected)
    pub models: Vec<String>,
}

/// IPC client for Gateway communication
pub struct GatewayClient {
    /// The endpoint URI to connect/reconnect to
    endpoint: String,
    /// The active transport connection (None when disconnected)
    conn: Option<Box<dyn AsyncTransportConnection>>,
    /// Request ID counter for correlating request/response
    next_request_id: u64,
    /// S4.5.2: Pending usage reports buffered during disconnect
    pending_reports: VecDeque<rollball_core::budget::UsageReport>,
}

impl GatewayClient {
    /// Create a new client that will connect to the given endpoint.
    ///
    /// The client starts disconnected. Call `connect()` to establish
    /// the transport connection.
    pub fn new(endpoint: &str) -> Self {
        let normalized = crate::ipc::transport::normalize_endpoint(endpoint);
        Self {
            endpoint: normalized,
            conn: None,
            next_request_id: 1,
            pending_reports: VecDeque::new(),
        }
    }

    /// Connect to the Gateway and send AgentHello to register.
    pub async fn connect(&mut self) -> Result<(), RollballError> {
        let conn = crate::ipc::transport::connect(&self.endpoint).await?;
        self.conn = Some(conn);
        tracing::info!("Connected to Gateway at: {}", self.endpoint);
        Ok(())
    }

    /// Connect to the Gateway and register with AgentHello.
    ///
    /// This is the preferred way to connect — it sends the AgentHello
    /// message after establishing the transport connection, so the
    /// Gateway can associate this connection with the agent.
    pub async fn connect_and_register(
        &mut self,
        agent_id: &str,
        version: &str,
    ) -> Result<(), RollballError> {
        self.connect_and_register_with_role(agent_id, version, "main").await
    }

    /// Connect to the Gateway and register with AgentHello, specifying connection role.
    ///
    /// `connection_role` is "main" for the primary IPC connection or
    /// "chunk-relay" for the streaming chunk relay connection.
    pub async fn connect_and_register_with_role(
        &mut self,
        agent_id: &str,
        version: &str,
        connection_role: &str,
    ) -> Result<(), RollballError> {
        self.connect().await?;

        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::AgentHelloResult { success: true, error: None }) => {
                tracing::info!("Gateway registered agent: {}", agent_id);
                Ok(())
            }
            Ok(GatewayResponse::AgentHelloResult { success: false, error: Some(e) }) => {
                tracing::error!("Gateway rejected AgentHello: {}", e);
                Err(RollballError::Ipc(format!("AgentHello rejected: {}", e)))
            }
            Ok(GatewayResponse::AgentHelloResult { success: false, error: None }) => {
                Err(RollballError::Ipc("AgentHello failed with no error".to_string()))
            }
            Ok(GatewayResponse::AgentHelloResult { success: true, error: Some(e) }) => {
                tracing::warn!("AgentHello succeeded but with error: {}", e);
                Ok(())
            }
            Ok(other) => {
                tracing::warn!("Unexpected AgentHello response: {:?}", other);
                Err(RollballError::Ipc(format!("Unexpected AgentHello response: {:?}", other)))
            }
            Err(e) => Err(e),
        }
    }

    /// Allocate a unique request ID
    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Check if the client is connected
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Disconnect from the Gateway
    pub async fn disconnect(&mut self) -> Result<(), RollballError> {
        self.conn = None;
        Ok(())
    }

    /// Receive a message from Gateway (blocking).
    ///
    /// This is used in the Gateway message loop to receive messages
    /// from the Gateway (like IntentReceived, system notifications).
    pub async fn recv_message(&mut self) -> Result<Option<GatewayResponse>, RollballError> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            RollballError::Ipc("Not connected to Gateway".to_string())
        })?;

        match conn.recv_frame().await? {
            Some(frame) => {
                let response: GatewayResponse = frame.to_message()?;
                Ok(Some(response))
            }
            None => Ok(None),
        }
    }

    /// Receive the LLM configuration from Gateway after handshake.
    ///
    /// After AgentHello, Gateway pushes LLMConfigDelivery containing
    /// the user's configured provider, model, and API key.
    /// This is the primary mechanism for distributing LLM credentials,
    /// satisfying PRD GTW-05 and SEC-07 (no env-var key distribution).
    ///
    /// If no LLMConfigDelivery is received within the timeout,
    /// returns an error and the caller falls back to manifest + env vars.
    pub async fn recv_llm_config(&mut self) -> Result<LlmConfigReceived, RollballError> {
        // Wait for LLMConfigDelivery with a timeout.
        // Gateway may also push IdentityDelivery and CapabilityOverview
        // during the handshake — skip those.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(RollballError::Ipc(
                    "Timeout waiting for LLMConfigDelivery from Gateway".to_string()
                ));
            }

            match tokio::time::timeout(remaining, self.recv_message()).await {
                Ok(Ok(Some(GatewayResponse::LLMConfigDelivery {
                    provider,
                    model,
                    api_key,
                    base_url,
                    models,
                }))) => {
                    tracing::info!(
                        provider = %provider,
                        model = ?model,
                        models = ?models,
                        "Received LLMConfigDelivery from Gateway"
                    );
                    return Ok(LlmConfigReceived {
                        provider,
                        model,
                        api_key: Some(api_key),
                        base_url,
                        models,
                    });
                }
                Ok(Ok(Some(_other))) => {
                    // Skip other handshake messages (IdentityDelivery, etc.)
                    tracing::debug!(
                        "Skipping non-LLMConfig message during handshake"
                    );
                    continue;
                }
                Ok(Ok(None)) => {
                    return Err(RollballError::Ipc(
                        "Connection closed while waiting for LLMConfigDelivery".to_string()
                    ));
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(RollballError::Ipc(
                        "Timeout waiting for LLMConfigDelivery from Gateway".to_string()
                    ));
                }
            }
        }
    }

    /// Send a request to Gateway and receive a response
    async fn send_and_recv(&mut self, request: GatewayRequest) -> Result<GatewayResponse, RollballError> {
        let _request_id = self.next_id();

        let conn = self.conn.as_mut().ok_or_else(|| {
            RollballError::Ipc("Not connected to Gateway".to_string())
        })?;

        // Create frame from request
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &request)
            .map_err(|e| RollballError::Ipc(format!("Failed to encode request: {e}")))?;

        // Send frame
        conn.send_frame(&frame).await?;

        // Receive response frame
        let response_frame = conn.recv_frame().await?
            .ok_or_else(|| RollballError::Ipc("Connection closed by Gateway".to_string()))?;

        // Decode response
        let response: GatewayResponse = response_frame
            .to_message()
            .map_err(|e| RollballError::Ipc(format!("Failed to decode response: {e}")))?;

        Ok(response)
    }

    /// Request an API key for a specific provider (KeyRelease)
    pub async fn request_key(&mut self, provider: &str) -> Result<String, RollballError> {
        let request = GatewayRequest::KeyRelease {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::KeyReleaseResult { api_key: Some(key), error: None }) => Ok(key),
            Ok(GatewayResponse::KeyReleaseResult { api_key: None, error: Some(e) }) => {
                Err(RollballError::Ipc(e))
            }
            Ok(GatewayResponse::KeyReleaseResult { api_key: None, error: None }) => {
                Err(RollballError::Ipc("KeyRelease returned no key and no error".to_string()))
            }
            Ok(GatewayResponse::KeyReleaseResult { api_key: Some(_), error: Some(_) }) => {
                Err(RollballError::Ipc("KeyRelease returned both key and error".to_string()))
            }
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// Send an Intent to another Agent
    pub async fn send_intent(
        &mut self,
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

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::IntentDelivered { message_id }) => Ok(message_id),
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// Send a streaming chunk via TYPE_STREAM_CHUNK frame (no response expected).
    ///
    /// This is the core of the IPC streaming protocol upgrade (Option B).
    /// Unlike `send_intent`, this uses Frame::TYPE_STREAM_CHUNK which tells
    /// the Gateway to process the chunk without sending a response frame back.
    /// This eliminates the per-chunk request-response round-trip overhead.
    ///
    /// The Gateway handles TYPE_STREAM_CHUNK by decoding the IntentSend body,
    /// broadcasting to the bridge channel, and NOT replying.
    pub async fn send_stream_chunk(
        &mut self,
        target: &str,
        action: &str,
        params: serde_json::Value,
        async_: bool,
    ) -> Result<(), RollballError> {
        let request = GatewayRequest::IntentSend {
            target: target.to_string(),
            action: action.to_string(),
            params,
            async_,
        };

        let conn = self.conn.as_mut().ok_or_else(|| {
            RollballError::Ipc("Not connected to Gateway".to_string())
        })?;

        let frame = Frame::from_message(Frame::TYPE_STREAM_CHUNK, &request)
            .map_err(|e| RollballError::Ipc(format!("Failed to encode stream chunk: {e}")))?;

        conn.send_frame(&frame).await?;
        // No recv_frame() — Gateway does not reply to TYPE_STREAM_CHUNK
        Ok(())
    }

    /// Query remaining budget for a provider
    pub async fn query_budget(
        &mut self,
        provider: &str,
    ) -> Result<(u64, f64), RollballError> {
        let request = GatewayRequest::BudgetQuery {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::BudgetInfo {
                remaining_tokens,
                remaining_cost_usd,
            }) => Ok((remaining_tokens, remaining_cost_usd)),
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// Report token usage to Gateway
    ///
    /// S4.5.1: AgentLoop step ⑦ calls this to send UsageReport.
    /// S4.5.2: If disconnected, the report is buffered and
    /// will be sent on reconnect via `flush_pending_reports`.
    pub async fn report_usage(
        &mut self,
        report: rollball_core::budget::UsageReport,
    ) -> Result<(), RollballError> {
        if !self.is_connected() {
            // S4.5.2: Buffer for later delivery
            if self.pending_reports.len() >= MAX_PENDING_REPORTS {
                self.pending_reports.pop_front(); // Drop oldest to make room
            }
            self.pending_reports.push_back(report);
            tracing::debug!(
                "Buffered usage report (disconnected), pending={}",
                self.pending_reports.len()
            );
            return Ok(());
        }

        let request = GatewayRequest::UsageReport(report);

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::UsageReportAck {}) => Ok(()),
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// S4.5.2: Flush pending usage reports after reconnect.
    ///
    /// Sends all buffered reports to the Gateway. Reports that fail
    /// to send are re-buffered for the next attempt.
    pub async fn flush_pending_reports(&mut self) -> Result<usize, RollballError> {
        // Take all pending reports out
        let reports: Vec<rollball_core::budget::UsageReport> =
            std::mem::take(&mut self.pending_reports).into_iter().collect();

        let mut sent = 0;
        for report in reports {
            let request = GatewayRequest::UsageReport(report.clone());
            match self.send_and_recv(request).await {
                Ok(GatewayResponse::UsageReportAck {}) => sent += 1,
                _ => {
                    // Re-buffer failed reports
                    if self.pending_reports.len() < MAX_PENDING_REPORTS {
                        self.pending_reports.push_back(report);
                    }
                }
            }
        }

        Ok(sent)
    }

    /// Get the number of pending usage reports
    pub fn pending_report_count(&self) -> usize {
        self.pending_reports.len()
    }

    /// Acquire a rate limit token
    pub async fn acquire_rate_token(
        &mut self,
        provider: &str,
    ) -> Result<(bool, Option<u64>), RollballError> {
        let request = GatewayRequest::RateAcquire {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::RateToken { granted, retry_after_ms }) => {
                Ok((granted, retry_after_ms))
            }
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// Request a runtime permission (S2.3)
    ///
    /// Sends a PermissionRequest to the Gateway when the PermissionChecker
    /// cache miss occurs and the permission policy requires user interaction.
    /// The request includes a unique request_id for correlation.
    ///
    /// Returns (granted, reason) on success. If the request times out
    /// or fails, returns (false, Some(error_message)).
    pub async fn request_permission(
        &mut self,
        permission: &str,
        reason: &str,
    ) -> Result<(bool, Option<String>), RollballError> {
        self.request_permission_with_timeout(permission, reason, rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS)
            .await
    }

    /// Request a runtime permission with a custom timeout (S2.3)
    pub async fn request_permission_with_timeout(
        &mut self,
        permission: &str,
        reason: &str,
        timeout_ms: u64,
    ) -> Result<(bool, Option<String>), RollballError> {
        let request_id = format!("perm-{}-{}", self.next_id(), chrono::Utc::now().timestamp_millis());

        let request = GatewayRequest::PermissionRequest {
            request_id: request_id.clone(),
            permission: permission.to_string(),
            reason: reason.to_string(),
            timeout_ms,
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::PermissionResult {
                request_id: resp_req_id,
                granted,
                reason,
            }) => {
                if resp_req_id != request_id {
                    tracing::warn!(
                        "PermissionResult request_id mismatch: expected={}, got={}",
                        request_id, resp_req_id
                    );
                }
                Ok((granted, reason))
            }
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }

    /// S4.2.4: Query capabilities from the Gateway
    pub async fn query_capabilities(
        &mut self,
        agent_id: Option<&str>,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, RollballError> {
        let request = GatewayRequest::CapabilityQuery {
            agent_id: agent_id.map(|s| s.to_string()),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::CapabilityOverview { capabilities }) => Ok(capabilities),
            Ok(other) => Err(RollballError::Ipc(format!("Unexpected response type: {:?}", other))),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_client_next_id() {
        let mut client = GatewayClient::new("unix:///tmp/test.sock");
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
    }

    #[test]
    fn test_gateway_client_not_connected() {
        let client = GatewayClient::new("unix:///tmp/test.sock");
        assert!(!client.is_connected());
    }

    #[test]
    fn test_gateway_client_normalize_endpoint() {
        let client = GatewayClient::new("/tmp/test.sock");
        assert_eq!(client.endpoint, "unix:///tmp/test.sock");

        let client = GatewayClient::new(r"\\.\pipe\test");
        assert_eq!(client.endpoint, r"pipe://\\.\pipe\test");
    }

    #[tokio::test]
    async fn test_gateway_client_not_connected_request() {
        let mut client = GatewayClient::new("unix:///tmp/test.sock");
        let result = client.request_key("openai").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not connected"));
    }

    #[tokio::test]
    async fn test_gateway_client_disconnect() {
        let mut client = GatewayClient::new("pipe://test");
        let result = client.disconnect().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_pending_report_buffering() {
        let client = GatewayClient::new("unix:///tmp/test.sock");
        // Client is not connected, so report_usage should buffer
        assert_eq!(client.pending_report_count(), 0);
    }
}
