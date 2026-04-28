//! Gateway Service API message definitions (contract layer, transport-agnostic)
//!
//! Defines the IPC protocol between Agent Runtime and Gateway.
//! All messages are JSON-serializable and transported via Frame format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::UsageReport;
use crate::identity::IdentityEntry;

/// Default timeout for runtime permission requests (60 seconds)
pub const PERMISSION_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// Default value helper for serde default attribute
fn default_permission_timeout() -> u64 {
    PERMISSION_REQUEST_TIMEOUT_MS
}

/// Default connection role for backward compatibility
fn default_connection_role() -> String {
    "main".to_string()
}

/// Gateway Service API request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayRequest {
    /// Request an API key for a specific provider
    KeyRelease { provider: String },
    /// Send an Intent to another Agent
    IntentSend {
        target: String,
        action: String,
        params: Value,
        #[serde(rename = "async")]
        async_: bool,
    },
    /// Query remaining budget for a provider
    BudgetQuery { provider: String },
    /// Report token usage
    UsageReport(UsageReport),
    /// Acquire a rate limit token
    RateAcquire { provider: String },
    /// Request a runtime permission (S2.1)
    ///
    /// Runtime sends this when PermissionChecker cache miss occurs
    /// and the permission policy requires user interaction.
    /// Gateway processes this in a separate tokio task to avoid
    /// blocking the IPC main loop.
    PermissionRequest {
        /// Unique request ID for correlating request/response
        request_id: String,
        /// Permission string (e.g., "filesystem:read:/etc")
        permission: String,
        /// Human-readable reason for the permission request
        reason: String,
        /// Timeout in milliseconds (default: 60000)
        #[serde(default = "default_permission_timeout")]
        timeout_ms: u64,
    },
    /// Query identity fields from System Agent
    IdentityQuery { fields: Vec<String> },
    /// Query capabilities for a specific agent or all agents
    CapabilityQuery {
        /// Optional agent ID filter (None = all agents)
        agent_id: Option<String>,
    },
    /// Register a cron entry (S3.4)
    CronRegister {
        /// Agent ID that owns this cron entry
        agent_id: String,
        /// Cron schedule expression (5-field)
        schedule: String,
        /// Action to fire when the schedule triggers
        action: String,
        /// Params to include in the IntentReceived
        params: Value,
    },
    /// Unregister a cron entry (S3.4)
    CronUnregister {
        /// Cron entry ID to remove
        cron_id: String,
    },
    /// List cron entries for the calling agent (S3.4)
    CronList {},
    /// Agent registration — first message sent after IPC connection
    /// Runtime sends this to identify itself to the Gateway
    AgentHello {
        /// The agent's reverse-domain identifier
        agent_id: String,
        /// The agent's version
        version: String,
        /// Connection role — "main" for the primary IPC connection,
        /// "chunk-relay" for the streaming chunk relay connection.
        /// The Gateway uses this to route IntentReceived only to "main" connections.
        /// Defaults to "main" when absent (backward compatible).
        #[serde(default = "default_connection_role")]
        connection_role: String,
    },
}

/// Gateway Service API response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayResponse {
    /// AgentHello response — confirms registration
    AgentHelloResult {
        /// Whether the registration was successful
        success: bool,
        /// Error message if registration failed
        error: Option<String>,
    },
    /// API key release result
    KeyReleaseResult {
        /// The released API key on success
        api_key: Option<String>,
        /// Error message on failure (e.g. "unauthenticated session", vault error)
        error: Option<String>,
    },
    /// Intent delivery confirmation
    IntentDelivered { message_id: String },
    /// Intent received from another Agent
    IntentReceived {
        from: String,
        action: String,
        params: Value,
    },
    /// Budget information
    BudgetInfo {
        remaining_tokens: u64,
        remaining_cost_usd: f64,
    },
    /// Usage report acknowledgment
    UsageReportAck {},
    /// Rate limit token
    RateToken {
        granted: bool,
        retry_after_ms: Option<u64>,
    },
    /// Permission request result (S2.1)
    ///
    /// Response to GatewayRequest::PermissionRequest.
    /// Includes request_id for correlation (important when
    /// multiple permission requests are in-flight).
    PermissionResult {
        /// Request ID from the original PermissionRequest
        request_id: String,
        /// Whether the permission was granted
        granted: bool,
        /// Reason for denial or additional info
        reason: Option<String>,
    },
    /// Identity delivery (Gateway → Runtime, cold-start injection)
    IdentityDelivery {
        /// List of identity entries from System Agent
        entries: Vec<IdentityEntry>,
    },
    /// LLM configuration delivery (Gateway → Runtime, handshake)
    ///
    /// After AgentHello, Gateway pushes the user's configured LLM provider
    /// to the Agent Runtime. This satisfies PRD GTW-05 and SEC-07:
    /// API keys are distributed via IPC, not environment variables.
    ///
    /// The provider always overrides the manifest's suggested_provider.
    /// model=None means Gateway has no model preference — Runtime falls back
    /// to the manifest's suggested_model.
    LLMConfigDelivery {
        /// Provider name (e.g. "minimax", "openai", "anthropic")
        provider: String,
        /// Model identifier (e.g. "MiniMax-M2.7", "minimax-m2.5").
        /// None when Gateway has no model preference — Runtime uses manifest's suggested_model.
        model: Option<String>,
        /// API key for the provider (one-time delivery, not stored on disk by Runtime)
        api_key: String,
        /// Base URL override (optional, provider-specific)
        base_url: Option<String>,
    },
    /// Identity query result from System Agent
    IdentityQueryResult {
        /// Field values
        values: std::collections::HashMap<String, String>,
        /// Confidence scores per field
        confidence: std::collections::HashMap<String, f32>,
    },
    /// Capability overview (handshake step ⑤ and CapabilityQuery response)
    CapabilityOverview {
        /// Map of agent_id → list of action names
        capabilities: std::collections::HashMap<String, Vec<String>>,
    },
    /// Capability update (incremental push on install/uninstall/update)
    CapabilityUpdate {
        /// Agent that was updated
        agent_id: String,
        /// New/updated actions
        actions: Vec<String>,
        /// Whether this is a removal
        removed: bool,
    },
    /// Cron registration result (S3.4)
    CronRegisterResult {
        /// Cron entry ID on success
        cron_id: Option<String>,
        /// Error message on failure
        error: Option<String>,
    },
    /// Cron unregistration result (S3.4)
    CronUnregisterResult {
        /// Whether the entry was found and removed
        removed: bool,
    },
    /// Cron list result (S3.4)
    CronListResult {
        /// List of cron entries
        entries: Vec<CronEntryInfo>,
    },
}

/// Cron entry info (for IPC responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntryInfo {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent ID that owns this entry
    pub agent_id: String,
    /// Cron schedule expression
    pub schedule: String,
    /// Action to fire
    pub action: String,
    /// Params for the IntentReceived
    pub params: Value,
}

/// Transport layer frame format
///
/// Wire format: `[body_len: u32 BE][msg_type: u8][body: JSON bytes]`
#[derive(Debug, Clone)]
pub struct Frame {
    /// Length of body in bytes (4 bytes big-endian on wire)
    pub body_len: u32,
    /// Message type discriminator
    pub msg_type: u8,
    /// JSON payload
    pub body: Vec<u8>,
}

impl Frame {
    /// Message type: request
    pub const TYPE_REQUEST: u8 = 0;
    /// Message type: response
    pub const TYPE_RESPONSE: u8 = 1;
    /// Message type: stream chunk
    pub const TYPE_STREAM_CHUNK: u8 = 2;
    /// Message type: error
    pub const TYPE_ERROR: u8 = 3;

    /// Wire header size: 4 bytes (body_len) + 1 byte (msg_type) = 5 bytes
    pub const HEADER_SIZE: usize = 5;

    /// Create a new Frame from JSON-serializable data
    pub fn from_message<T: Serialize>(msg_type: u8, msg: &T) -> Result<Self, serde_json::Error> {
        let body = serde_json::to_vec(msg)?;
        let body_len = body.len() as u32;
        Ok(Self {
            body_len,
            msg_type,
            body,
        })
    }

    /// Decode Frame body into typed message
    pub fn to_message<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// Encode frame to wire format bytes
    ///
    /// Wire format: `[body_len: u32 BE][msg_type: u8][body: JSON bytes]`
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::HEADER_SIZE + self.body.len());
        buf.extend_from_slice(&self.body_len.to_be_bytes());
        buf.push(self.msg_type);
        buf.extend_from_slice(&self.body);
        buf
    }

    /// Decode frame from wire format bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self, FrameError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(FrameError::TooShort {
                expected: Self::HEADER_SIZE,
                actual: data.len(),
            });
        }

        let body_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let msg_type = data[4];
        let body = data[Self::HEADER_SIZE..].to_vec();

        if body.len() != body_len as usize {
            return Err(FrameError::LengthMismatch {
                expected: body_len as usize,
                actual: body.len(),
            });
        }

        Ok(Self {
            body_len,
            msg_type,
            body,
        })
    }
}

/// Frame encoding/decoding errors
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("Frame too short: expected at least {expected} bytes, got {actual}")]
    TooShort { expected: usize, actual: usize },
    #[error("Body length mismatch: expected {expected} bytes, got {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_request_serialize_key_release() {
        let req = GatewayRequest::KeyRelease {
            provider: "openai".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"KeyRelease\""));
        assert!(json.contains("\"provider\":\"openai\""));
    }

    #[test]
    fn test_gateway_request_roundtrip() {
        let req = GatewayRequest::IntentSend {
            target: "com.example.calendar".into(),
            action: "schedule".into(),
            params: serde_json::json!({"time": "10:00"}),
            async_: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: GatewayRequest = serde_json::from_str(&json).unwrap();
        if let GatewayRequest::IntentSend {
            target, action, ..
        } = parsed
        {
            assert_eq!(target, "com.example.calendar");
            assert_eq!(action, "schedule");
        } else {
            panic!("Expected IntentSend variant");
        }
    }

    #[test]
    fn test_gateway_response_roundtrip() {
        let resp = GatewayResponse::BudgetInfo {
            remaining_tokens: 50000,
            remaining_cost_usd: 1.5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::BudgetInfo {
            remaining_tokens, ..
        } = parsed
        {
            assert_eq!(remaining_tokens, 50000);
        } else {
            panic!("Expected BudgetInfo variant");
        }
    }

    #[test]
    fn test_frame_from_message() {
        let req = GatewayRequest::KeyRelease {
            provider: "openai".into(),
        };
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &req).unwrap();
        assert_eq!(frame.msg_type, Frame::TYPE_REQUEST);
        assert!(frame.body_len > 0);
        assert!(!frame.body.is_empty());
    }

    #[test]
    fn test_frame_to_message() {
        let req = GatewayRequest::KeyRelease {
            provider: "openai".into(),
        };
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &req).unwrap();
        let parsed: GatewayRequest = frame.to_message().unwrap();
        if let GatewayRequest::KeyRelease { provider } = parsed {
            assert_eq!(provider, "openai");
        } else {
            panic!("Expected KeyRelease variant");
        }
    }

    #[test]
    fn test_frame_wire_format_roundtrip() {
        let req = GatewayRequest::RateAcquire {
            provider: "anthropic".into(),
        };
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &req).unwrap();
        let wire_bytes = frame.to_bytes();

        // Verify header
        assert!(wire_bytes.len() >= Frame::HEADER_SIZE);
        assert_eq!(wire_bytes[4], Frame::TYPE_REQUEST);

        // Decode back
        let decoded = Frame::from_bytes(&wire_bytes).unwrap();
        assert_eq!(decoded.msg_type, Frame::TYPE_REQUEST);
        assert_eq!(decoded.body_len, frame.body_len);

        let parsed: GatewayRequest = decoded.to_message().unwrap();
        if let GatewayRequest::RateAcquire { provider } = parsed {
            assert_eq!(provider, "anthropic");
        } else {
            panic!("Expected RateAcquire variant");
        }
    }

    #[test]
    fn test_frame_from_bytes_too_short() {
        let result = Frame::from_bytes(&[0u8; 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_from_bytes_length_mismatch() {
        // body_len says 100 but only 0 body bytes
        let mut data = vec![0u8; 5];
        data[0..4].copy_from_slice(&100u32.to_be_bytes());
        data[4] = Frame::TYPE_REQUEST;
        let result = Frame::from_bytes(&data);
        assert!(result.is_err());
    }

    // ── S2.1: PermissionRequest/PermissionResult protocol tests ──────

    #[test]
    fn test_permission_request_serialization() {
        let req = GatewayRequest::PermissionRequest {
            request_id: "req-001".to_string(),
            permission: "filesystem:read:/etc".to_string(),
            reason: "Need to read config".to_string(),
            timeout_ms: PERMISSION_REQUEST_TIMEOUT_MS,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"PermissionRequest\""));
        assert!(json.contains("\"request_id\":\"req-001\""));
        assert!(json.contains("\"permission\":\"filesystem:read:/etc\""));
        assert!(json.contains("\"timeout_ms\":60000"));
    }

    #[test]
    fn test_permission_request_roundtrip() {
        let req = GatewayRequest::PermissionRequest {
            request_id: "req-002".to_string(),
            permission: "shell".to_string(),
            reason: "Execute build script".to_string(),
            timeout_ms: 30_000,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: GatewayRequest = serde_json::from_str(&json).unwrap();
        if let GatewayRequest::PermissionRequest {
            request_id,
            permission,
            reason,
            timeout_ms,
        } = parsed
        {
            assert_eq!(request_id, "req-002");
            assert_eq!(permission, "shell");
            assert_eq!(reason, "Execute build script");
            assert_eq!(timeout_ms, 30_000);
        } else {
            panic!("Expected PermissionRequest variant");
        }
    }

    #[test]
    fn test_permission_request_default_timeout() {
        // When timeout_ms is missing from JSON, it should default to 60000
        let json = r#"{"type":"PermissionRequest","request_id":"req-003","permission":"network:https://api.example.com","reason":"API call"}"#;
        let parsed: GatewayRequest = serde_json::from_str(json).unwrap();
        if let GatewayRequest::PermissionRequest { timeout_ms, .. } = parsed {
            assert_eq!(timeout_ms, PERMISSION_REQUEST_TIMEOUT_MS);
        } else {
            panic!("Expected PermissionRequest variant");
        }
    }

    #[test]
    fn test_permission_result_serialization() {
        let resp = GatewayResponse::PermissionResult {
            request_id: "req-001".to_string(),
            granted: true,
            reason: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"PermissionResult\""));
        assert!(json.contains("\"request_id\":\"req-001\""));
        assert!(json.contains("\"granted\":true"));
    }

    #[test]
    fn test_permission_result_roundtrip() {
        let resp = GatewayResponse::PermissionResult {
            request_id: "req-004".to_string(),
            granted: false,
            reason: Some("User denied".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::PermissionResult {
            request_id,
            granted,
            reason,
        } = parsed
        {
            assert_eq!(request_id, "req-004");
            assert!(!granted);
            assert_eq!(reason.unwrap(), "User denied");
        } else {
            panic!("Expected PermissionResult variant");
        }
    }
}
