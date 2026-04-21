//! Inbound message queue for external message injection
//!
//! Allows external sources (Gateway, cross-agent intents, system notifications)
//! to inject messages into the agent loop without blocking the main execution.

use serde::{Deserialize, Serialize};

/// Maximum payload size in bytes for inbound message text content.
/// Messages exceeding this limit will be truncated.
const MAX_INBOUND_PAYLOAD_SIZE: usize = 4096;

/// Truncate a string to at most `max_bytes` bytes, ensuring we don't split
/// a multi-byte UTF-8 character.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Find the last valid char boundary within max_bytes
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Truncate a serde_json::Value's string representation to `max_bytes` bytes,
/// ensuring we don't split a multi-byte UTF-8 character.
fn truncate_json_to_bytes(val: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    let s = val.to_string();
    if s.len() <= max_bytes {
        return val.clone();
    }
    let truncated = truncate_to_bytes(&s, max_bytes);
    serde_json::Value::String(truncated)
}

/// Messages that can be injected into the agent loop from external sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InboundMessage {
    /// Direct user message
    UserMessage(String),
    /// System notification (identity update, capability change, etc.)
    SystemNotification {
        notification_type: String,
        data: serde_json::Value,
    },
    /// Cross-agent intent message
    IntentMessage {
        from: String,
        action: String,
        params: serde_json::Value,
    },
}

impl InboundMessage {
    /// Apply payload size limits to this message, truncating if necessary.
    /// Returns the (possibly truncated) message and whether truncation occurred.
    pub fn enforce_size_limit(mut self) -> (Self, bool) {
        let mut truncated = false;
        match &mut self {
            InboundMessage::UserMessage(text) => {
                if text.len() > MAX_INBOUND_PAYLOAD_SIZE {
                    tracing::warn!(
                        original_len = text.len(),
                        max = MAX_INBOUND_PAYLOAD_SIZE,
                        "Truncating oversized inbound UserMessage"
                    );
                    *text = truncate_to_bytes(text, MAX_INBOUND_PAYLOAD_SIZE);
                    truncated = true;
                }
            }
            InboundMessage::SystemNotification { notification_type, data } => {
                let data_str = data.to_string();
                let header_overhead = notification_type.len() + "[system:] ".len();
                let effective_limit = MAX_INBOUND_PAYLOAD_SIZE.saturating_sub(header_overhead);
                if data_str.len() > effective_limit {
                    tracing::warn!(
                        original_len = data_str.len(),
                        max = effective_limit,
                        notification_type = %notification_type,
                        "Truncating oversized inbound SystemNotification data"
                    );
                    *data = truncate_json_to_bytes(data, effective_limit);
                    truncated = true;
                }
            }
            InboundMessage::IntentMessage { from, action, params } => {
                let params_str = params.to_string();
                let header_overhead = from.len() + action.len() + "[intent::] ".len();
                let effective_limit = MAX_INBOUND_PAYLOAD_SIZE.saturating_sub(header_overhead);
                if params_str.len() > effective_limit {
                    tracing::warn!(
                        original_len = params_str.len(),
                        max = effective_limit,
                        from = %from,
                        action = %action,
                        "Truncating oversized inbound IntentMessage params"
                    );
                    *params = truncate_json_to_bytes(params, effective_limit);
                    truncated = true;
                }
            }
        }
        (self, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbound_message_serde_roundtrip() {
        let msg = InboundMessage::IntentMessage {
            from: "com.rollball.system".to_string(),
            action: "update_identity".to_string(),
            params: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: InboundMessage = serde_json::from_str(&json).unwrap();
        if let InboundMessage::IntentMessage { from, action, params } = decoded {
            assert_eq!(from, "com.rollball.system");
            assert_eq!(action, "update_identity");
            assert_eq!(params["key"], "value");
        } else {
            panic!("Expected IntentMessage");
        }
    }

    #[test]
    fn test_truncate_to_bytes_ascii() {
        let s = "Hello, world!";
        assert_eq!(truncate_to_bytes(s, 100), s);
        assert_eq!(truncate_to_bytes(s, 5), "Hello");
    }

    #[test]
    fn test_truncate_to_bytes_utf8_multibyte() {
        // "你好世界" — each CJK char is 3 bytes
        let s = "你好世界";
        assert_eq!(s.len(), 12); // 4 chars * 3 bytes
        // Truncate to 8 bytes — should land on char boundary (2 chars * 3 = 6 bytes)
        let result = truncate_to_bytes(s, 8);
        assert_eq!(result, "你好");
        // Truncate at exact char boundary
        let result = truncate_to_bytes(s, 6);
        assert_eq!(result, "你好");
        // Truncate at 7 bytes — should fall back to 6 (end of second char)
        let result = truncate_to_bytes(s, 7);
        assert_eq!(result, "你好");
    }

    #[test]
    fn test_truncate_to_bytes_emoji() {
        // "🎉🎊" — emoji are 4 bytes each in UTF-8
        let s = "🎉🎊";
        assert_eq!(s.len(), 8);
        // Truncate to 5 bytes — should fall back to 4 (first emoji only)
        let result = truncate_to_bytes(s, 5);
        assert_eq!(result, "🎉");
    }

    #[test]
    fn test_enforce_size_limit_small_message() {
        let msg = InboundMessage::UserMessage("Hello".to_string());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(!truncated);
        if let InboundMessage::UserMessage(text) = msg {
            assert_eq!(text, "Hello");
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_oversized_user_message() {
        let long_text = "A".repeat(MAX_INBOUND_PAYLOAD_SIZE + 1000);
        let msg = InboundMessage::UserMessage(long_text.clone());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::UserMessage(text) = msg {
            assert!(text.len() <= MAX_INBOUND_PAYLOAD_SIZE);
            assert!(text.starts_with('A'));
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_utf8_safe_truncation() {
        // Create a string with multi-byte chars that exceeds the limit
        let long_text = "你好".repeat(2000); // Each 你好 is 6 bytes, total > 4096
        let msg = InboundMessage::UserMessage(long_text.clone());
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::UserMessage(text) = msg {
            // Verify the truncated string is valid UTF-8
            assert!(text.len() <= MAX_INBOUND_PAYLOAD_SIZE);
            assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        } else {
            panic!("Expected UserMessage");
        }
    }

    #[test]
    fn test_enforce_size_limit_system_notification() {
        let big_data = serde_json::json!({"key": "A".repeat(MAX_INBOUND_PAYLOAD_SIZE + 500)});
        let msg = InboundMessage::SystemNotification {
            notification_type: "test".to_string(),
            data: big_data,
        };
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::SystemNotification { data, .. } = msg {
            // Data should have been truncated
            assert!(data.to_string().len() <= MAX_INBOUND_PAYLOAD_SIZE);
        } else {
            panic!("Expected SystemNotification");
        }
    }

    #[test]
    fn test_enforce_size_limit_intent_message() {
        let big_params = serde_json::json!({"data": "B".repeat(MAX_INBOUND_PAYLOAD_SIZE + 500)});
        let msg = InboundMessage::IntentMessage {
            from: "com.rollball.system".to_string(),
            action: "ping".to_string(),
            params: big_params,
        };
        let (msg, truncated) = msg.enforce_size_limit();
        assert!(truncated);
        if let InboundMessage::IntentMessage { params, .. } = msg {
            assert!(params.to_string().len() <= MAX_INBOUND_PAYLOAD_SIZE);
        } else {
            panic!("Expected IntentMessage");
        }
    }
}
