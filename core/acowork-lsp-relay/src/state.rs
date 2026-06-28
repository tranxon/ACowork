//! LSP Relay state machine.
//!
//! Defines the high-level state of the LSP relay process, published
//! via the event bus for the Gateway supervisor to monitor.

use serde::{Deserialize, Serialize};

/// High-level state of the LSP relay process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LspRelayState {
    /// Process is starting up, config not yet loaded.
    Starting,
    /// Config loaded, HTTP server bound, ready to serve.
    Ready {
        /// Number of configured languages.
        language_count: usize,
    },
    /// Fatal error — cannot recover.
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_starting_serializes_to_snake_case() {
        let json = serde_json::to_string(&LspRelayState::Starting).unwrap();
        assert_eq!(json, r#""starting""#);
    }

    #[test]
    fn test_ready_serializes_with_language_count() {
        let state = LspRelayState::Ready { language_count: 7 };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""ready""#), "expected snake_case tag: {json}");
        assert!(json.contains(r#""language_count":7"#), "expected count: {json}");
    }

    #[test]
    fn test_error_serializes_with_message() {
        let state = LspRelayState::Error {
            message: "config load failed".to_string(),
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""error""#), "expected snake_case tag: {json}");
        assert!(
            json.contains(r#""message":"config load failed""#),
            "expected message: {json}"
        );
    }

    #[test]
    fn test_state_round_trip() {
        let original = LspRelayState::Ready { language_count: 3 };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: LspRelayState = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_state_equality() {
        assert_eq!(LspRelayState::Starting, LspRelayState::Starting);
        assert_eq!(
            LspRelayState::Ready { language_count: 1 },
            LspRelayState::Ready { language_count: 1 }
        );
        assert_ne!(
            LspRelayState::Ready { language_count: 1 },
            LspRelayState::Ready { language_count: 2 }
        );
        assert_ne!(
            LspRelayState::Starting,
            LspRelayState::Error {
                message: "x".into()
            }
        );
    }
}
