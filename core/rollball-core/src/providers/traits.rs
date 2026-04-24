//! Provider trait and chat message types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Classification of LLM provider error conditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProviderErrorType {
    /// HTTP 429 — rate limited by the provider.
    RateLimited,
    /// HTTP 401/403 — authentication or authorization failure.
    Unauthorized,
    /// HTTP 5xx — transient server-side failure.
    ServerError,
    /// HTTP 4xx (other) — client-side request error.
    ClientError,
    /// Network-level failure (timeout, DNS, connection reset, etc.).
    NetworkError,
    /// Unclassified error.
    Unknown,
}

/// Structured error type for LLM provider failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    /// Human-readable error message.
    pub message: String,
    /// HTTP status code, if the error originated from an HTTP response.
    pub status_code: Option<u16>,
    /// Classified error type.
    pub error_type: ProviderErrorType,
    /// Whether the request may succeed if retried.
    pub retryable: bool,
}

impl ProviderError {
    /// Build a `ProviderError` from an HTTP status code and message.
    /// Automatically infers `error_type` and `retryable`.
    pub fn from_status_code(status: u16, message: String) -> Self {
        let (error_type, retryable) = match status {
            429 => (ProviderErrorType::RateLimited, true),
            401 | 403 => (ProviderErrorType::Unauthorized, false),
            500..=599 => (ProviderErrorType::ServerError, true),
            400..=499 => (ProviderErrorType::ClientError, false),
            _ => (ProviderErrorType::Unknown, false),
        };
        Self {
            message,
            status_code: Some(status),
            error_type,
            retryable,
        }
    }

    /// Convenience constructor for network-level errors.
    pub fn network(message: String) -> Self {
        Self {
            message,
            status_code: None,
            error_type: ProviderErrorType::NetworkError,
            retryable: true,
        }
    }

    /// Convenience constructor for unclassified errors.
    pub fn unknown(message: String) -> Self {
        Self {
            message,
            status_code: None,
            error_type: ProviderErrorType::Unknown,
            retryable: false,
        }
    }

    /// Convenience constructor for authentication/authorization errors.
    pub fn unauthorized(message: String) -> Self {
        Self {
            message,
            status_code: Some(401),
            error_type: ProviderErrorType::Unauthorized,
            retryable: false,
        }
    }

    /// Convenience constructor for rate-limited errors.
    pub fn rate_limited(message: String) -> Self {
        Self {
            message,
            status_code: Some(429),
            error_type: ProviderErrorType::RateLimited,
            retryable: true,
        }
    }

    /// Convenience constructor for server-side errors.
    pub fn server_error(message: String) -> Self {
        Self {
            message,
            status_code: Some(500),
            error_type: ProviderErrorType::ServerError,
            retryable: true,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.status_code {
            write!(f, "[{}] {} (retryable={})", code, self.message, self.retryable)
        } else {
            write!(f, "{} (retryable={})", self.message, self.retryable)
        }
    }
}

impl std::error::Error for ProviderError {}


/// Chat message role
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Chat message in conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Tool call from LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Chat request to LLM provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
}

/// Chat response from LLM provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

/// Usage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Stream event for streaming responses
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text chunk
    Content(String),
    /// Tool call start
    ToolCallStart(ToolCall),
    /// Tool call chunk
    ToolCallChunk(String),
    /// Stream finished
    Finished(ChatResponse),
    /// Error occurred
    Error(String),
}

/// Provider trait for LLM providers
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get provider name
    fn name(&self) -> &str;

    /// Send a chat request and get response
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Send a chat request with streaming response
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn futures_core::Stream<Item = StreamEvent> + Send>>;

    /// Count tokens in a message (approximate)
    async fn chat_token_count(&self, messages: &[ChatMessage]) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_error_from_status_429() {
        let err = ProviderError::from_status_code(429, "Too many requests".to_string());
        assert_eq!(err.status_code, Some(429));
        assert_eq!(err.error_type, ProviderErrorType::RateLimited);
        assert!(err.retryable);
        assert_eq!(err.message, "Too many requests");
    }

    #[test]
    fn test_provider_error_from_status_401() {
        let err = ProviderError::from_status_code(401, "Unauthorized".to_string());
        assert_eq!(err.status_code, Some(401));
        assert_eq!(err.error_type, ProviderErrorType::Unauthorized);
        assert!(!err.retryable);
    }

    #[test]
    fn test_provider_error_from_status_500() {
        let err = ProviderError::from_status_code(500, "Internal server error".to_string());
        assert_eq!(err.status_code, Some(500));
        assert_eq!(err.error_type, ProviderErrorType::ServerError);
        assert!(err.retryable);
    }

    #[test]
    fn test_provider_error_from_status_400() {
        let err = ProviderError::from_status_code(400, "Bad request".to_string());
        assert_eq!(err.status_code, Some(400));
        assert_eq!(err.error_type, ProviderErrorType::ClientError);
        assert!(!err.retryable);
    }

    #[test]
    fn test_provider_error_network() {
        let err = ProviderError::network("Connection timed out".to_string());
        assert_eq!(err.status_code, None);
        assert_eq!(err.error_type, ProviderErrorType::NetworkError);
        assert!(err.retryable);
        assert_eq!(err.message, "Connection timed out");
    }

    #[test]
    fn test_provider_error_display() {
        let err = ProviderError::from_status_code(429, "Rate limited".to_string());
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
        assert!(display.contains("retryable=true"));

        let err_no_code = ProviderError::network("DNS error".to_string());
        let display_no_code = format!("{err_no_code}");
        assert!(display_no_code.contains("DNS error"));
        assert!(display_no_code.contains("retryable=true"));
    }
}
