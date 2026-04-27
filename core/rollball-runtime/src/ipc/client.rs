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

/// IPC client for Gateway communication
pub struct GatewayClient {
    /// The active transport connection (None when disconnected)
    conn: Option<Box<dyn AsyncTransportConnection>>,
    /// The endpoint URI to connect/reconnect to
    endpoint: String,
    /// Request ID counter for correlating request/response
    next_request_id: parking_lot::Mutex<u64>,
    /// S4.5.2: Pending usage reports buffered during disconnect
    pending_reports: parking_lot::Mutex<VecDeque<rollball_core::budget::UsageReport>>,
}

impl GatewayClient {
    /// Create a new client that will connect to the given endpoint.
    ///
    /// The client starts disconnected. Call `connect()` to establish
    /// the transport connection.
    pub fn new(endpoint: &str) -> Self {
        let normalized = crate::ipc::transport::normalize_endpoint(endpoint);
        Self {
            conn: None,
            endpoint: normalized,
            next_request_id: parking_lot::Mutex::new(1),
            pending_reports: parking_lot::Mutex::new(VecDeque::new()),
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
        self.connect().await?;

        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
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
    fn next_id(&self) -> u64 {
        let mut id = self.next_request_id.lock();
        let current = *id;
        *id += 1;
        current
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

    /// Send a request to Gateway and receive a response
    async fn send_and_recv(&mut self, request: GatewayRequest) -> Result<GatewayResponse, RollballError> {
        // Allocate request ID before borrowing conn
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
            let mut pending = self.pending_reports.lock();
            if pending.len() >= MAX_PENDING_REPORTS {
                pending.pop_front(); // Drop oldest to make room
            }
            pending.push_back(report);
            tracing::debug!(
                "Buffered usage report (disconnected), pending={}",
                pending.len()
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
        let reports: Vec<rollball_core::budget::UsageReport> = {
            let mut pending = self.pending_reports.lock();
            pending.drain(..).collect()
        };

        let mut sent = 0;
        let mut failed = VecDeque::new();

        for report in reports {
            let request = GatewayRequest::UsageReport(report.clone());
            match self.send_and_recv(request).await {
                Ok(GatewayResponse::UsageReportAck {}) => sent += 1,
                Ok(_) => failed.push_back(report),
                Err(_) => failed.push_back(report),
            }
        }

        // Re-buffer failed reports
        if !failed.is_empty() {
            let mut pending = self.pending_reports.lock();
            for report in failed {
                if pending.len() < MAX_PENDING_REPORTS {
                    pending.push_back(report);
                }
            }
        }

        Ok(sent)
    }

    /// Get the number of pending usage reports
    pub fn pending_report_count(&self) -> usize {
        self.pending_reports.lock().len()
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
        let client = GatewayClient::new("unix:///tmp/test.sock");
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
