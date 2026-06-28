//! LSP protocol helpers — frame parsing and JSON-RPC message inspection.
//!
//! These are pure functions with no side effects, extracted from the
//! Gateway's LSP module for reuse in the standalone LSP relay.

/// Parse `Content-Length: N` from a header line.
pub fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim();
    let prefix = "Content-Length:";
    if let Some(rest) = line.strip_prefix(prefix) {
        rest.trim().parse().ok()
    } else if let Some(rest) = line.strip_prefix("Content-length:") {
        rest.trim().parse().ok()
    } else {
        None
    }
}

/// Extract the JSON-RPC "method" field from a message for diagnostic logging.
pub fn extract_method_hint(msg: &str) -> String {
    if let Some(idx) = msg.find("\"method\":") {
        let rest = &msg[idx + 9..];
        if let Some(open) = rest.find('"')
            && let Some(close) = rest[open + 1..].find('"')
        {
            return rest[open + 1..open + 1 + close].to_string();
        }
    }
    if msg.contains("\"id\":") && !msg.contains("\"method\":") {
        return "(response)".to_string();
    }
    "(no method)".to_string()
}

/// Check if a message is an LSP `initialize` request.
pub fn is_initialize_request(msg: &str) -> bool {
    msg.contains("\"method\":\"initialize\"")
}

/// Check if a message is an LSP `initialized` notification.
pub fn is_initialized_notification(msg: &str) -> bool {
    msg.contains("\"method\":\"initialized\"")
}

/// Check if a message is an LSP `InitializeResult` (response with capabilities).
pub fn is_initialize_result(msg: &str) -> bool {
    !msg.contains("\"method\":") && msg.contains("\"id\":") && msg.contains("\"capabilities\"")
}

/// Extract the JSON-RPC `id` field from a message.
pub fn extract_jsonrpc_id(msg: &str) -> String {
    if let Some(idx) = msg.find("\"id\":") {
        let rest = &msg[idx + 5..];
        let rest = rest.trim_start();
        if let Some(stripped) = rest.strip_prefix('"') {
            if let Some(close) = stripped.find('"') {
                return rest[..close + 2].to_string();
            }
        } else {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            return rest[..end].to_string();
        }
    }
    "0".to_string()
}

/// Substitute the `id` field in a cached JSON-RPC response with a new id.
pub fn substitute_jsonrpc_id(msg: &str, new_id: &str) -> String {
    if let Some(idx) = msg.find("\"id\":") {
        let before = &msg[..idx + 5];
        let rest = &msg[idx + 5..];
        let rest_trimmed = rest.trim_start();
        let whitespace = &rest[..rest.len() - rest_trimmed.len()];

        if let Some(stripped) = rest_trimmed.strip_prefix('"') {
            if let Some(close) = stripped.find('"') {
                let after = &rest_trimmed[close + 2..];
                return format!("{}{}{}{}", before, whitespace, new_id, after);
            }
        } else {
            let end = rest_trimmed
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest_trimmed.len());
            let after = &rest_trimmed[end..];
            return format!("{}{}{}{}", before, whitespace, new_id, after);
        }
    }
    msg.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_length_standard() {
        assert_eq!(parse_content_length("Content-Length: 42"), Some(42));
        assert_eq!(parse_content_length("Content-Length:0"), Some(0));
        assert_eq!(parse_content_length("Content-Length: 1234\r\n"), Some(1234));
    }

    #[test]
    fn test_parse_content_length_lowercase() {
        assert_eq!(parse_content_length("Content-length: 99"), Some(99));
    }

    #[test]
    fn test_parse_content_length_invalid() {
        assert_eq!(parse_content_length("Content-Type: application/json"), None);
        assert_eq!(parse_content_length("X-Custom: 42"), None);
        assert_eq!(parse_content_length(""), None);
    }

    #[test]
    fn test_parse_content_length_not_a_number() {
        assert_eq!(parse_content_length("Content-Length: abc"), None);
    }

    #[test]
    fn test_is_initialize_request() {
        assert!(is_initialize_request(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#
        ));
        assert!(!is_initialize_request(
            r#"{"jsonrpc":"2.0","method":"shutdown","id":2}"#
        ));
    }

    #[test]
    fn test_is_initialized_notification() {
        assert!(is_initialized_notification(
            r#"{"jsonrpc":"2.0","method":"initialized"}"#
        ));
        assert!(!is_initialized_notification(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#
        ));
    }

    #[test]
    fn test_is_initialize_result() {
        assert!(is_initialize_result(
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#
        ));
        assert!(!is_initialize_result(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#
        ));
    }

    #[test]
    fn test_extract_jsonrpc_id_numeric() {
        assert_eq!(
            extract_jsonrpc_id(r#"{"jsonrpc":"2.0","id":42,"method":"test"}"#),
            "42"
        );
    }

    #[test]
    fn test_extract_jsonrpc_id_string() {
        assert_eq!(
            extract_jsonrpc_id(r#"{"jsonrpc":"2.0","id":"abc","method":"test"}"#),
            "\"abc\""
        );
    }

    #[test]
    fn test_substitute_jsonrpc_id() {
        let original = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let patched = substitute_jsonrpc_id(original, "42");
        assert!(patched.contains("\"id\":42"));
        assert!(patched.contains("\"capabilities\""));
    }

    #[test]
    fn test_extract_method_hint_with_method() {
        assert_eq!(
            extract_method_hint(r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#),
            "initialize"
        );
        assert_eq!(
            extract_method_hint(r#"{"jsonrpc":"2.0","method":"textDocument/hover","id":2}"#),
            "textDocument/hover"
        );
        assert_eq!(
            extract_method_hint(r#"{"jsonrpc":"2.0","method":"workspace/symbol"}"#),
            "workspace/symbol"
        );
    }

    #[test]
    fn test_extract_method_hint_response() {
        // A response has "id" but no "method"
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        assert_eq!(extract_method_hint(msg), "(response)");
    }

    #[test]
    fn test_extract_method_hint_no_method_no_id() {
        // Neither method nor id — should return "(no method)"
        let msg = r#"{"jsonrpc":"2.0","result":{}}"#;
        assert_eq!(extract_method_hint(msg), "(no method)");
    }

    #[test]
    fn test_extract_method_hint_with_spaces() {
        // Method field with spaces around the colon
        let msg = r#"{"jsonrpc":"2.0", "method": "shutdown", "id": 3}"#;
        assert_eq!(extract_method_hint(msg), "shutdown");
    }
}
