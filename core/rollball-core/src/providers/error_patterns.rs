//! Error pattern matching for LLM provider errors.
//!
//! Provides centralized pattern matching for classifying raw error strings
//! into structured types (`StreamError`, `ProviderErrorType`). This is the
//! single source of truth for "is this a context overflow?", "is this a
//! stream decode error?", etc.
//!
//! All upstream consumers (ReliableProvider, AgentLoop, etc.) should use
//! these functions instead of implementing their own string matching.

use crate::providers::traits::{ProviderErrorType, StreamError};

/// Check if an error message indicates a context window / token limit overflow.
///
/// Covers patterns from Anthropic, OpenAI, MiniMax, DeepSeek, Ollama,
/// OpenRouter, llama.cpp, Mistral, and generic HTTP 413 responses.
pub fn is_context_overflow(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        // Anthropic
        "exceeds the context window",
        "exceeds the available context size",
        "context window of this model",
        // OpenAI
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
        "prompt exceeds max length",
        // OpenRouter / DeepSeek / vLLM
        "max context length",
        // MiniMax / OpenAI API error code
        "context_length_exceeded",
        // HTTP 413
        "request entity too large",
        // Ollama
        "exceeded max context length",
        // Mistral
        "too large for model",
        // llama.cpp
        "exceeds the available context",
        // z.ai
        "model_context_window_exceeded",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates a stream decode / transport error.
///
/// These are typically caused by HTTP/2 frame errors, connection resets,
/// or mid-stream data corruption — all potentially recoverable by retry.
pub fn is_stream_decode_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        "error decoding response body",
        "error decoding",
        "connection reset",
        "broken pipe",
        "stream error",
        "h2 protocol error",
        "http2 frame error",
        "unexpected eof",
        "connection closed",
        "stream was reset",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates an authentication / authorization failure.
///
/// These are NOT retryable without user intervention (changing API key, etc.).
pub fn is_auth_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        "unauthorized",
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates a non-retryable rate limit
/// (business/quota exhaustion that retries cannot fix).
pub fn is_non_retryable_rate_limit(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const BUSINESS_PATTERNS: &[&str] = &[
        "plan does not include",
        "insufficient balance",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "model not available for your plan",
        "free usage limit",
    ];
    // Known provider business codes: 1113 (MiniMax), 1311
    if lower.contains("1113") || lower.contains("1311") {
        return true;
    }
    BUSINESS_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Classify a raw stream error string into a structured `StreamError`.
///
/// This is the unified entry point for converting unstructured error
/// strings (e.g. from SSE parsing failures) into structured types that
/// carry `error_type` and `retryable` metadata.
pub fn classify_stream_error(msg: &str) -> StreamError {
    if is_context_overflow(msg) {
        StreamError::context_overflow(msg.to_string())
    } else if is_stream_decode_error(msg) {
        StreamError::stream_decode(msg.to_string())
    } else if is_auth_error(msg) {
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::Unauthorized,
            retryable: false,
            status_code: None,
        }
    } else if is_non_retryable_rate_limit(msg) {
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::PaymentRequired,
            retryable: false,
            status_code: None,
        }
    } else {
        // Default: unknown errors are NOT retryable.
        // Treat conservatively — unknown errors could be permanent failures
        // (e.g. server-side rejection) and retrying would waste budget.
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::Unknown,
            retryable: false,
            status_code: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_overflow_detection() {
        assert!(is_context_overflow("prompt exceeds the context window of this model"));
        assert!(is_context_overflow("Prompt is too long: 150000 tokens"));
        assert!(is_context_overflow("context_length_exceeded"));
        assert!(is_context_overflow("Request entity too large"));
        assert!(!is_context_overflow("connection timed out"));
    }

    #[test]
    fn test_stream_decode_error_detection() {
        assert!(is_stream_decode_error("error decoding response body"));
        assert!(is_stream_decode_error("Stream error: connection reset by peer"));
        assert!(is_stream_decode_error("h2 protocol error"));
        assert!(!is_stream_decode_error("prompt is too long"));
    }

    #[test]
    fn test_auth_error_detection() {
        assert!(is_auth_error("Unauthorized: invalid api key"));
        assert!(is_auth_error("403 Forbidden"));
        assert!(!is_auth_error("connection reset"));
    }

    #[test]
    fn test_non_retryable_rate_limit_detection() {
        assert!(is_non_retryable_rate_limit("insufficient_quota"));
        assert!(is_non_retryable_rate_limit("Error code 1113"));
        assert!(!is_non_retryable_rate_limit("Too many requests"));
    }

    #[test]
    fn test_classify_stream_error_context_overflow() {
        let err = classify_stream_error("prompt exceeds the context window");
        assert_eq!(err.error_type, ProviderErrorType::ContextOverflow);
        assert!(!err.retryable);
    }

    #[test]
    fn test_classify_stream_error_decode() {
        let err = classify_stream_error("error decoding response body");
        assert_eq!(err.error_type, ProviderErrorType::StreamDecodeError);
        assert!(err.retryable);
    }

    #[test]
    fn test_classify_stream_error_auth() {
        let err = classify_stream_error("invalid api key provided");
        assert_eq!(err.error_type, ProviderErrorType::Unauthorized);
        assert!(!err.retryable);
    }

    #[test]
    fn test_classify_stream_error_unknown_non_retryable() {
        let err = classify_stream_error("some unknown transient error");
        assert_eq!(err.error_type, ProviderErrorType::Unknown);
        assert!(!err.retryable);
    }
}
