//! Gateway Service API message definitions (contract layer, transport-agnostic)
//!
//! Defines the IPC protocol between Agent Runtime and Gateway.
//! All messages are JSON-serializable and transported via Frame format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::UsageReport;
use crate::identity::IdentityEntry;

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
    /// Request a runtime permission
    PermissionRequest { permission: String, reason: String },
    /// Query identity fields from System Agent
    IdentityQuery { fields: Vec<String> },
    /// Query capabilities for a specific agent or all agents
    CapabilityQuery {
        /// Optional agent ID filter (None = all agents)
        agent_id: Option<String>,
    },
}

/// Gateway Service API response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayResponse {
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
    /// Permission request result
    PermissionResult {
        granted: bool,
        reason: Option<String>,
    },
    /// Identity delivery (Gateway → Runtime, cold-start injection)
    IdentityDelivery {
        /// List of identity entries from System Agent
        entries: Vec<IdentityEntry>,
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
}
