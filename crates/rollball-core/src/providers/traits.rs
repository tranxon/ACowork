//! Provider trait and chat message types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

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
