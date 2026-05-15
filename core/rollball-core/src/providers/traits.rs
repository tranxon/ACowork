//! Provider trait and chat message types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Classification of LLM provider error conditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProviderErrorType {
    /// HTTP 429 — rate limited by the provider (retryable after backoff).
    RateLimited,
    /// HTTP 402 — payment required / insufficient quota (NOT retryable).
    PaymentRequired,
    /// HTTP 401/403 — authentication or authorization failure.
    Unauthorized,
    /// HTTP 5xx — transient server-side failure.
    ServerError,
    /// HTTP 4xx (other) — client-side request error.
    ClientError,
    /// Network-level failure (timeout, DNS, connection reset, etc.).
    NetworkError,
    /// Context window / token limit exceeded.
    /// Not directly retryable — requires history trimming before retry.
    ContextOverflow,
    /// Stream decode error (mid-stream data corruption, h2 frame error, etc.).
    /// Retryable — re-issuing the same request may succeed.
    StreamDecodeError,
    /// Stream silence — no data received within per-chunk read timeout.
    /// Retryable — the provider may have been temporarily overloaded.
    StreamTimeout,
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
            402 => (ProviderErrorType::PaymentRequired, false),
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

    /// Convenience constructor for payment required / insufficient quota errors.
    /// These are NOT retryable — the user needs to add funds or upgrade their plan.
    pub fn payment_required(message: String) -> Self {
        Self {
            message,
            status_code: Some(402),
            error_type: ProviderErrorType::PaymentRequired,
            retryable: false,
        }
    }

    /// Convenience constructor for context window / token limit overflow errors.
    /// Not directly retryable — requires history trimming before retry.
    pub fn context_overflow(message: String) -> Self {
        Self {
            message,
            status_code: None,
            error_type: ProviderErrorType::ContextOverflow,
            retryable: false,
        }
    }

    /// Convenience constructor for stream decode errors (mid-stream data corruption).
    /// These are retryable — re-issuing the same request may succeed.
    pub fn stream_decode(message: String) -> Self {
        Self {
            message,
            status_code: None,
            error_type: ProviderErrorType::StreamDecodeError,
            retryable: true,
        }
    }

    /// Convenience constructor for stream silence / read timeout errors.
    /// These are retryable — the provider may have been temporarily overloaded.
    pub fn stream_timeout(timeout_secs: u64) -> Self {
        Self {
            message: format!(
                "Stream timeout: no data received for {}s",
                timeout_secs
            ),
            status_code: None,
            error_type: ProviderErrorType::StreamTimeout,
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

/// Structured error for stream events.
///
/// Unlike `ProviderError` (which represents call-level failures before a stream
/// is established), `StreamError` represents failures that occur *during* an
/// active SSE stream. It carries the same classification metadata so that
/// upstream consumers (ReliableProvider, AgentLoop) can make informed retry
/// decisions without string matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamError {
    /// Human-readable error message.
    pub message: String,
    /// Classified error type.
    pub error_type: ProviderErrorType,
    /// Whether re-issuing the same request may succeed.
    pub retryable: bool,
    /// HTTP status code, if the error originated from an HTTP response.
    pub status_code: Option<u16>,
}

impl StreamError {
    /// Create a `StreamError` from a `ProviderError`, preserving all metadata.
    pub fn from_provider_error(err: &ProviderError) -> Self {
        Self {
            message: err.message.clone(),
            error_type: err.error_type.clone(),
            retryable: err.retryable,
            status_code: err.status_code,
        }
    }

    /// Convenience constructor for stream decode errors (mid-stream data corruption).
    /// These are retryable — re-issuing the same request may succeed.
    pub fn stream_decode(message: String) -> Self {
        Self {
            message,
            error_type: ProviderErrorType::StreamDecodeError,
            retryable: true,
            status_code: None,
        }
    }

    /// Convenience constructor for stream silence / read timeout errors.
    /// These are retryable — the provider may have been temporarily overloaded.
    pub fn stream_timeout(timeout_secs: u64) -> Self {
        Self {
            message: format!(
                "Stream timeout: no data received for {}s",
                timeout_secs
            ),
            error_type: ProviderErrorType::StreamTimeout,
            retryable: true,
            status_code: None,
        }
    }

    /// Convenience constructor for context window / token limit overflow errors.
    /// Not directly retryable — requires history trimming before retry.
    pub fn context_overflow(message: String) -> Self {
        Self {
            message,
            error_type: ProviderErrorType::ContextOverflow,
            retryable: false,
            status_code: None,
        }
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.status_code {
            write!(
                f,
                "[{}] {} (type={:?}, retryable={})",
                code, self.message, self.error_type, self.retryable
            )
        } else {
            write!(
                f,
                "{} (type={:?}, retryable={})",
                self.message, self.error_type, self.retryable
            )
        }
    }
}

impl std::error::Error for StreamError {}


/// Chat message role
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// Chat message in conversation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    #[serde(default)]
    pub role: MessageRole,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool call ID — required for role=Tool messages to match the corresponding
    /// assistant tool_call. Maps to the `tool_call_id` field in the OpenAI API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl ChatMessage {
    /// Create a user message
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: MessageRole::User, content: content.into(), ..Default::default() }
    }

    /// Create an assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: MessageRole::Assistant, content: content.into(), ..Default::default() }
    }

    /// Create an assistant message with reasoning content (DeepSeek thinking mode)
    pub fn assistant_with_reasoning(content: impl Into<String>, reasoning: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            reasoning_content: Some(reasoning.into()),
            ..Default::default()
        }
    }

    /// Create an assistant message with tool calls
    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            ..Default::default()
        }
    }

    /// Create a tool result message
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    /// Create a system message
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: MessageRole::System, content: content.into(), ..Default::default() }
    }
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatResponse {
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
    /// Unix timestamp ms when reasoning/think started (set by streaming loop)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_started_at: Option<i64>,
    /// Unix timestamp ms when reasoning/think finished (set by streaming loop)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_finished_at: Option<i64>,
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
    /// Reasoning content chunk (e.g. DeepSeek thinking mode)
    ReasoningContent(String),
    /// Tool call start
    ToolCallStart(ToolCall),
    /// Tool call argument chunk (index identifies which tool call, arguments is incremental JSON fragment)
    ToolCallChunk { index: u64, arguments: String },
    /// Stream finished
    Finished(ChatResponse),
    /// Error occurred during streaming. Carries structured metadata so that
    /// upstream consumers (ReliableProvider, AgentLoop) can make informed retry
    /// decisions without string matching.
    Error(StreamError),
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

    #[test]
    fn test_provider_error_from_status_402() {
        let err = ProviderError::from_status_code(402, "Insufficient quota".to_string());
        assert_eq!(err.status_code, Some(402));
        assert_eq!(err.error_type, ProviderErrorType::PaymentRequired);
        assert!(!err.retryable);
        assert_eq!(err.message, "Insufficient quota");
    }

    #[test]
    fn test_provider_error_payment_required_constructor() {
        let err = ProviderError::payment_required("Billing limit reached".to_string());
        assert_eq!(err.status_code, Some(402));
        assert_eq!(err.error_type, ProviderErrorType::PaymentRequired);
        assert!(!err.retryable);
        assert_eq!(err.message, "Billing limit reached");
    }
}
