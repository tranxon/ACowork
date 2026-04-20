//! Gateway Service API server
//!
//! Accepts IPC connections from Agent Runtime processes,
//! decodes requests, routes to handlers, and sends responses.

use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use crate::error::GatewayError;
use crate::ipc::session::SessionManager;
use crate::ipc::transport::{TransportConnection, create_transport};
use crate::gateway::state::GatewayState;

/// IPC server
pub struct IpcServer {
    socket_path: String,
    session_mgr: SessionManager,
}

impl IpcServer {
    /// Create new IPC server
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            session_mgr: SessionManager::new(),
        }
    }

    /// Start the server (blocking)
    ///
    /// Phase 1 limitation: this is a synchronous, single-connection server.
    /// Only one Agent Runtime can be connected at a time; the second must
    /// wait for the first to disconnect. Phase 2 will use async I/O to
    /// support multiple concurrent connections.
    pub fn run(&mut self, state: &mut GatewayState) -> Result<(), GatewayError> {
        let transport = create_transport(&self.socket_path)?;
        transport.listen()?;
        tracing::info!("IPC server listening on: {}", self.socket_path);

        loop {
            let mut conn = transport.accept()?;
            let conn_id = format!("conn-{}", self.session_mgr.session_count() + 1);
            tracing::info!("Accepted connection: {}", conn_id);

            self.session_mgr.create_session(&conn_id);
            
            // Handle connection
            if let Err(e) = self.handle_connection(&conn_id, &mut conn, state) {
                tracing::warn!("Connection {} error: {}", conn_id, e);
            }
            
            conn.close().ok();
            self.session_mgr.remove_session(&conn_id);
            tracing::info!("Connection {} closed", conn_id);
        }
    }

    /// Handle a single connection's messages
    fn handle_connection(
        &mut self,
        conn_id: &str,
        conn: &mut Box<dyn TransportConnection>,
        state: &mut GatewayState,
    ) -> Result<(), GatewayError> {
        loop {
            let frame = match conn.recv_frame()? {
                Some(f) => f,
                None => return Ok(()), // Connection closed
            };

            if frame.msg_type == Frame::TYPE_REQUEST {
                let request: GatewayRequest = frame.to_message()
                    .map_err(|e| GatewayError::Ipc(format!("Failed to decode request: {}", e)))?;
                
                tracing::debug!("Received request from {}: {:?}", conn_id, request);

                let response = self.handle_request(request, conn_id, state);
                
                let resp_frame = Frame::from_message(Frame::TYPE_RESPONSE, &response)
                    .map_err(|e| GatewayError::Ipc(format!("Failed to encode response: {}", e)))?;
                
                conn.send_frame(&resp_frame)?;
            }
        }
    }

    /// Route request to appropriate handler
    fn handle_request(
        &mut self,
        request: GatewayRequest,
        conn_id: &str,
        state: &mut GatewayState,
    ) -> GatewayResponse {
        match request {
            GatewayRequest::KeyRelease { provider } => {
                self.handle_key_release(&provider, conn_id, state)
            }
            GatewayRequest::IntentSend { target, action, params, async_ } => {
                self.handle_intent_send(&target, &action, &params, async_, conn_id)
            }
            GatewayRequest::BudgetQuery { provider } => {
                self.handle_budget_query(&provider)
            }
            GatewayRequest::UsageReport(report) => {
                self.handle_usage_report(report)
            }
            GatewayRequest::RateAcquire { provider } => {
                self.handle_rate_acquire(&provider)
            }
            GatewayRequest::PermissionRequest { permission, reason } => {
                self.handle_permission_request(&permission, &reason)
            }
        }
    }

    fn handle_key_release(
        &mut self,
        provider: &str,
        conn_id: &str,
        state: &mut GatewayState,
    ) -> GatewayResponse {
        // Check if session is authenticated
        let session = self.session_mgr.get_session_mut(conn_id);
        let agent_id = session.and_then(|s| s.agent_id.clone());

        match agent_id {
            Some(id) => {
                // Look up API key from vault
                match state.vault.get_key(provider) {
                    Ok(api_key) => {
                        tracing::info!("KeyRelease for agent={}, provider={}", id, provider);
                        GatewayResponse::KeyReleaseResult { api_key }
                    }
                    Err(e) => {
                        tracing::warn!("KeyRelease failed for agent={}, provider={}: {}", id, provider, e);
                        GatewayResponse::KeyReleaseResult { api_key: String::new() }
                    }
                }
            }
            None => {
                tracing::warn!("KeyRelease from unauthenticated session {}", conn_id);
                GatewayResponse::KeyReleaseResult { api_key: String::new() }
            }
        }
    }

    fn handle_intent_send(
        &mut self,
        target: &str,
        action: &str,
        _params: &serde_json::Value,
        async_: bool,
        conn_id: &str,
    ) -> GatewayResponse {
        let session = self.session_mgr.get_session(conn_id);
        let from = session.and_then(|s| s.agent_id.clone())
            .unwrap_or_else(|| "unknown".to_string());

        tracing::info!(
            "IntentSend from={} to={} action={} async={}",
            from, target, action, async_
        );

        // Phase 1: just acknowledge. Phase 2: route to target agent.
        let message_id = format!("msg-{}", chrono::Utc::now().timestamp_millis());
        GatewayResponse::IntentDelivered { message_id }
    }

    fn handle_budget_query(&self, _provider: &str) -> GatewayResponse {
        // Phase 1: return placeholder budget
        GatewayResponse::BudgetInfo {
            remaining_tokens: 100_000,
            remaining_cost_usd: 10.0,
        }
    }

    fn handle_usage_report(&self, _report: rollball_core::budget::UsageReport) -> GatewayResponse {
        // Phase 1: just acknowledge
        GatewayResponse::UsageReportAck {}
    }

    fn handle_rate_acquire(&self, _provider: &str) -> GatewayResponse {
        // Phase 1: always grant
        GatewayResponse::RateToken {
            granted: true,
            retry_after_ms: None,
        }
    }

    fn handle_permission_request(&self, permission: &str, reason: &str) -> GatewayResponse {
        // Phase 1: always deny runtime permission requests (need user UI)
        tracing::warn!("PermissionRequest denied: {} (reason: {})", permission, reason);
        GatewayResponse::PermissionResult {
            granted: false,
            reason: Some("Runtime permission requests not supported in Phase 1".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_server_new() {
        let server = IpcServer::new("/tmp/test.sock");
        assert_eq!(server.socket_path, "/tmp/test.sock");
        assert_eq!(server.session_mgr.session_count(), 0);
    }

    #[test]
    fn test_handle_budget_query() {
        let server = IpcServer::new("/tmp/test.sock");
        let response = server.handle_budget_query("openai");
        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
            assert_eq!(remaining_tokens, 100_000);
        } else {
            panic!("Expected BudgetInfo");
        }
    }

    #[test]
    fn test_handle_rate_acquire() {
        let server = IpcServer::new("/tmp/test.sock");
        let response = server.handle_rate_acquire("openai");
        if let GatewayResponse::RateToken { granted, retry_after_ms } = response {
            assert!(granted);
            assert!(retry_after_ms.is_none());
        } else {
            panic!("Expected RateToken");
        }
    }

    #[test]
    fn test_handle_permission_request() {
        let server = IpcServer::new("/tmp/test.sock");
        let response = server.handle_permission_request("filesystem:read:/etc", "need config");
        if let GatewayResponse::PermissionResult { granted, reason } = response {
            assert!(!granted);
            assert!(reason.is_some());
        } else {
            panic!("Expected PermissionResult");
        }
    }

    #[test]
    fn test_handle_intent_send() {
        let mut server = IpcServer::new("/tmp/test.sock");
        let response = server.handle_intent_send(
            "com.example.calendar",
            "schedule",
            &serde_json::json!({"time": "10:00"}),
            false,
            "conn-1",
        );
        if let GatewayResponse::IntentDelivered { message_id } = response {
            assert!(!message_id.is_empty());
        } else {
            panic!("Expected IntentDelivered");
        }
    }

    #[test]
    fn test_handle_usage_report() {
        let server = IpcServer::new("/tmp/test.sock");
        let report = rollball_core::budget::UsageReport {
            agent_id: "com.example.weather".to_string(),
            provider: "openai".to_string(),
            tokens_used: 150,
            cost_usd: 0.01,
            timestamp: chrono::Utc::now(),
            error: None,
        };
        let response = server.handle_usage_report(report);
        assert!(matches!(response, GatewayResponse::UsageReportAck {}));
    }
}
