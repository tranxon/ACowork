//! Gateway Service API client
//!
//! IPC client for communicating with the Gateway process.
//! Supports KeyRelease, IntentSend, BudgetQuery, UsageReport,
//! RateAcquire, and PermissionRequest.
//! S4.5.2: Buffers pending usage reports when disconnected,
//! and replays them on reconnect.

use std::collections::VecDeque;
use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use crate::ipc::transport::Transport;

/// Maximum number of pending usage reports to buffer
const MAX_PENDING_REPORTS: usize = 100;

/// IPC client for Gateway communication
pub struct GatewayClient {
    transport: Box<dyn Transport>,
    /// Request ID counter for correlating request/response
    next_request_id: parking_lot::Mutex<u64>,
    /// S4.5.2: Pending usage reports buffered during disconnect
    pending_reports: parking_lot::Mutex<VecDeque<rollball_core::budget::UsageReport>>,
}

impl GatewayClient {
    /// Create new client with the given transport
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            next_request_id: parking_lot::Mutex::new(1),
            pending_reports: parking_lot::Mutex::new(VecDeque::new()),
        }
    }

    /// Create a client connected to the given endpoint
    pub fn connect(endpoint: &str) -> Result<Self, String> {
        let transport = crate::ipc::transport::create_transport(endpoint);
        Ok(Self::new(transport))
    }

    /// Allocate a unique request ID
    fn next_id(&self) -> u64 {
        let mut id = self.next_request_id.lock();
        let current = *id;
        *id += 1;
        current
    }

    /// Connect the underlying transport to the endpoint
    pub async fn connect_transport(&self, endpoint: &str) -> Result<(), String> {
        self.transport.connect(endpoint).await
    }

    /// Check if the client is connected
    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    /// Disconnect from the Gateway
    pub async fn disconnect(&self) -> Result<(), String> {
        self.transport.disconnect().await
    }

    /// Send a request to Gateway and receive a response
    async fn send_and_recv(&self, request: GatewayRequest) -> Result<GatewayResponse, String> {
        if !self.is_connected() {
            return Err("Not connected to Gateway".to_string());
        }

        let _request_id = self.next_id();

        // Create frame from request
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &request)
            .map_err(|e| format!("Failed to encode request: {e}"))?;

        // Send frame
        self.transport.send_frame(&frame).await?;

        // Receive response frame
        let response_frame = self.transport.recv_frame().await?;

        // Decode response
        let response: GatewayResponse = response_frame
            .to_message()
            .map_err(|e| format!("Failed to decode response: {e}"))?;

        Ok(response)
    }

    /// Request an API key for a specific provider (KeyRelease)
    pub async fn request_key(&self, provider: &str) -> Result<String, String> {
        let request = GatewayRequest::KeyRelease {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::KeyReleaseResult { api_key: Some(key), error: None }) => Ok(key),
            Ok(GatewayResponse::KeyReleaseResult { api_key: None, error: Some(e) }) => Err(e),
            Ok(GatewayResponse::KeyReleaseResult { api_key: None, error: None }) => {
                Err("KeyRelease returned no key and no error".to_string())
            }
            Ok(GatewayResponse::KeyReleaseResult { api_key: Some(_), error: Some(_) }) => {
                Err("KeyRelease returned both key and error".to_string())
            }
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// Send an Intent to another Agent
    pub async fn send_intent(
        &self,
        target: &str,
        action: &str,
        params: serde_json::Value,
        async_: bool,
    ) -> Result<String, String> {
        let request = GatewayRequest::IntentSend {
            target: target.to_string(),
            action: action.to_string(),
            params,
            async_,
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::IntentDelivered { message_id }) => Ok(message_id),
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// Query remaining budget for a provider
    pub async fn query_budget(
        &self,
        provider: &str,
    ) -> Result<(u64, f64), String> {
        let request = GatewayRequest::BudgetQuery {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::BudgetInfo {
                remaining_tokens,
                remaining_cost_usd,
            }) => Ok((remaining_tokens, remaining_cost_usd)),
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// Report token usage to Gateway
    ///
    /// S4.5.1: AgentLoop step ⑦ calls this to send UsageReport.
    /// S4.5.2: If disconnected, the report is buffered and
    /// will be sent on reconnect via `flush_pending_reports`.
    pub async fn report_usage(
        &self,
        report: rollball_core::budget::UsageReport,
    ) -> Result<(), String> {
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
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// S4.5.2: Flush pending usage reports after reconnect.
    ///
    /// Sends all buffered reports to the Gateway. Reports that fail
    /// to send are re-buffered for the next attempt.
    pub async fn flush_pending_reports(&self) -> Result<usize, String> {
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
        &self,
        provider: &str,
    ) -> Result<(bool, Option<u64>), String> {
        let request = GatewayRequest::RateAcquire {
            provider: provider.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::RateToken { granted, retry_after_ms }) => {
                Ok((granted, retry_after_ms))
            }
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// Request a runtime permission
    pub async fn request_permission(
        &self,
        permission: &str,
        reason: &str,
    ) -> Result<(bool, Option<String>), String> {
        let request = GatewayRequest::PermissionRequest {
            permission: permission.to_string(),
            reason: reason.to_string(),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::PermissionResult { granted, reason }) => {
                Ok((granted, reason))
            }
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }

    /// S4.2.4: Query capabilities from the Gateway
    pub async fn query_capabilities(
        &self,
        agent_id: Option<&str>,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
        let request = GatewayRequest::CapabilityQuery {
            agent_id: agent_id.map(|s| s.to_string()),
        };

        match self.send_and_recv(request).await {
            Ok(GatewayResponse::CapabilityOverview { capabilities }) => Ok(capabilities),
            Ok(other) => Err(format!("Unexpected response type: {:?}", other)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_client_next_id() {
        let transport = crate::ipc::transport::create_transport("unix:///tmp/test.sock");
        let client = GatewayClient::new(transport);
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
    }

    #[test]
    fn test_gateway_client_connect() {
        let result = GatewayClient::connect("unix:///tmp/test.sock");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_client_not_connected() {
        let transport = crate::ipc::transport::create_transport("unix:///tmp/test.sock");
        let client = GatewayClient::new(transport);
        assert!(!client.is_connected());

        let result = client.request_key("openai").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not connected"));
    }

    #[tokio::test]
    async fn test_gateway_client_disconnect() {
        let transport = crate::ipc::transport::create_transport("pipe://\\\\.\\pipe\\test");
        let client = GatewayClient::new(transport);
        // Even without connecting, disconnect should not error
        let result = client.disconnect().await;
        assert!(result.is_ok());
    }
}
