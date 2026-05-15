//! Anthropic Claude Provider
//!
//! Supports the Anthropic Messages API (claude-sonnet-4, claude-haiku, etc.)
//! with streaming, tool_use, and structured error handling.

use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

/// Per-chunk read timeout: if no data arrives within this duration,
/// the stream is considered stalled and we emit a StreamTimeout error.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(45);

use rollball_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall, MessageRole, Provider, StreamEvent,
    ToolCall, UsageInfo,
};

// ── Provider struct ──────────────────────────────────────────────────────

/// Anthropic Claude provider
pub struct AnthropicProvider {
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with default base URL
    pub fn new(api_key: Option<&str>) -> Self {
        Self::with_base_url(None, api_key)
    }

    /// Create provider with custom base URL
    pub fn with_base_url(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            http_client,
        }
    }

    /// Set API key after construction (e.g., from Vault KeyRelease)
    pub fn set_api_key(&mut self, key: String) {
        self.api_key = Some(key);
    }
}

// ── Anthropic API types ──────────────────────────────────────────────────

/// Anthropic Messages API request
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

/// Anthropic message format
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
}

/// Anthropic tool specification
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicToolSpec {
    name: String,
    description: String,
    #[serde(rename = "input_schema")]
    input_schema: serde_json::Value,
}

/// Anthropic Messages API response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Anthropic content block
#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// Anthropic usage information
#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// Anthropic error response
#[derive(Debug, Deserialize)]
struct AnthropicError {
    error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

// ── Streaming SSE types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamEventRaw {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ContentBlockStart>,
    #[serde(default)]
    message: Option<AnthropicResponse>,
    /// Usage info included in message_delta event
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamDelta {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ContentBlockStart {
    #[serde(rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

// ── Conversion helpers ──────────────────────────────────────────────────

/// Convert our ChatMessage list to Anthropic format.
/// Anthropic requires system messages to be passed separately,
/// and tool messages to use `tool_result` content blocks.
fn convert_messages(messages: &[ChatMessage]) -> (Vec<AnthropicMessage>, Option<String>) {
    let mut system_prompt = None;
    let mut converted = Vec::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                // Anthropic uses a top-level `system` field
                system_prompt = Some(msg.content.clone());
            }
            MessageRole::User => {
                converted.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::Value::Array(vec![serde_json::json!({
                        "type": "text",
                        "text": msg.content
                    })])),
                });
            }
            MessageRole::Assistant => {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();

                // Add text content if present
                if !msg.content.is_empty() {
                    content_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": msg.content
                    }));
                }

                // Add tool_use blocks if present
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input
                        }));
                    }
                }

                converted.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: if content_blocks.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Array(content_blocks))
                    },
                });
            }
            MessageRole::Tool => {
                // Anthropic tool results: user message with tool_result content block
                // Prefer dedicated tool_call_id field; fall back to content JSON for legacy
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_else(|| {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                        value.get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string()
                    } else {
                        "unknown".to_string()
                    }
                });
                let result_content = if msg.tool_call_id.is_some() {
                    // New format: content is the actual result
                    msg.content.clone()
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    // Legacy format: content JSON contains tool_call_id and content
                    value.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&msg.content)
                        .to_string()
                } else {
                    msg.content.clone()
                };

                converted.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::Value::Array(vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": result_content
                    })])),
                });
            }
        }
    }

    (converted, system_prompt)
}

/// Convert tools from our format to Anthropic format
fn convert_tools(tools: Option<&[serde_json::Value]>) -> Option<Vec<AnthropicToolSpec>> {
    tools.map(|items| {
        items
            .iter()
            .map(|tool| {
                let name = tool["name"].as_str().unwrap_or("unknown").to_string();
                tracing::debug!(
                    tool = %name,
                    has_parameters = tool.get("parameters").is_some(),
                    has_input_schema = tool.get("input_schema").is_some(),
                    tool_keys = ?tool.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                    "Anthropic convert_tools field check"
                );
                AnthropicToolSpec {
                    name,
                    description: tool
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type": "object", "properties": {}})),
                }
            })
            .collect()
    })
}

/// Parse Anthropic response into our ChatResponse
fn parse_response(resp: AnthropicResponse) -> ChatResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in resp.content {
        match block.block_type.as_str() {
            "text" => {
                if let Some(text) = block.text {
                    text_parts.push(text);
                }
            }
            "tool_use" => {
                tool_calls.push(ToolCall {
                    id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: block.name.unwrap_or_default(),
                        arguments: block
                            .input
                            .map(|v| serde_json::to_string(&v).unwrap_or_default())
                            .unwrap_or_default(),
                    },
                });
            }
            _ => {}
        }
    }

    let usage_info = resp.usage.map(|u| {
        let input = u.input_tokens.unwrap_or(0);
        let output = u.output_tokens.unwrap_or(0);
        let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = u.cache_read_input_tokens.unwrap_or(0);
        UsageInfo {
            prompt_tokens: input + cache_creation + cache_read,
            completion_tokens: output,
            total_tokens: input + cache_creation + cache_read + output,
        }
    });

    ChatResponse {
        content: text_parts.join(""),
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: usage_info,
        ..Default::default()
    }
}

// ── Provider trait implementation ───────────────────────────────────────

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(&self, request: ChatRequest) -> rollball_core::error::Result<ChatResponse> {
        let (messages, system) = convert_messages(&request.messages);

        let anthropic_request = AnthropicRequest {
            model: request.model,
            messages,
            system,
            max_tokens: request.max_tokens.or(Some(4096)),
            temperature: request.temperature,
            tools: convert_tools(request.tools.as_deref()),
            stream: None,
        };

        // Log request payload for debugging tool definitions
        tracing::debug!(
            request_len = serde_json::to_string(&anthropic_request).map(|s| s.len()).unwrap_or(0),
            model = %anthropic_request.model,
            has_tools = anthropic_request.tools.is_some(),
            "Anthropic chat request"
        );

        let url = format!("{}/messages", self.base_url);

        let api_key = self.api_key.as_ref().ok_or_else(|| {
            rollball_core::RollballError::Provider(rollball_core::ProviderError::unauthorized(
                "Anthropic API key is required".to_string(),
            ))
        })?;

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| {
                rollball_core::RollballError::Provider(rollball_core::ProviderError::network(
                    format!("Anthropic request failed: {e}"),
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            // Try to parse Anthropic error structure for better classification
            let message = if let Ok(err_resp) = serde_json::from_str::<AnthropicError>(&body) {
                format!(
                    "Anthropic API error [{}]: {}",
                    err_resp.error.error_type, err_resp.error.message
                )
            } else {
                format!("Anthropic API error: {status} — {body}")
            };

            return Err(rollball_core::RollballError::Provider(
                rollball_core::ProviderError::from_status_code(status.as_u16(), message),
            ));
        }

        let native_resp: AnthropicResponse = response.json().await.map_err(|e| {
            rollball_core::RollballError::Provider(rollball_core::ProviderError::unknown(
                format!("Failed to parse Anthropic response: {e}"),
            ))
        })?;

        Ok(parse_response(native_resp))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> rollball_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (messages, system) = convert_messages(&request.messages);

        let anthropic_request = AnthropicRequest {
            model: request.model,
            messages,
            system,
            max_tokens: request.max_tokens.or(Some(4096)),
            temperature: request.temperature,
            tools: convert_tools(request.tools.as_deref()),
            stream: Some(true),
        };

        // Log request payload for debugging tool definitions
        tracing::debug!(
            request_len = serde_json::to_string(&anthropic_request).map(|s| s.len()).unwrap_or(0),
            model = %anthropic_request.model,
            has_tools = anthropic_request.tools.is_some(),
            "Anthropic chat_stream request"
        );

        let url = format!("{}/messages", self.base_url);

        let api_key = self.api_key.as_ref().ok_or_else(|| {
            rollball_core::RollballError::Provider(rollball_core::ProviderError::unauthorized(
                "Anthropic API key is required".to_string(),
            ))
        })?;

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| {
                rollball_core::RollballError::Provider(
                    rollball_core::ProviderError::network(format!(
                        "Anthropic streaming request failed: {e}"
                    )),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(rollball_core::RollballError::Provider(
                rollball_core::ProviderError::from_status_code(
                    status.as_u16(),
                    format!("Anthropic API error: {status} — {body}"),
                ),
            ));
        }

        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut pending_tool_id: Option<String> = None;
            let mut pending_tool_name: Option<String> = None;
            let mut accumulated_input_tokens: u64 = 0;
            let mut block_index_map: HashMap<u64, u64> = HashMap::new();

            use futures_util::StreamExt;
            loop {
                match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer[..newline_pos].to_string();
                            buffer = buffer[newline_pos + 1..].to_string();

                            if let Some(event) = parse_anthropic_sse_line(
                                &line,
                                &mut pending_tool_id,
                                &mut pending_tool_name,
                                &mut accumulated_input_tokens,
                                &mut block_index_map,
                            )
                                && tx.send(Some(event)).await.is_err()
                            {
                                return; // receiver dropped
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        let stream_err = rollball_core::providers::classify_stream_error(
                            &format!("Stream error: {e}"),
                        );
                        let _ = tx.send(Some(StreamEvent::Error(stream_err))).await;
                        return;
                    }
                    Ok(None) => {
                        // Stream ended normally
                        break;
                    }
                    Err(_) => {
                        // Stream silence — no data received within read timeout
                        tracing::warn!(
                            timeout_secs = STREAM_READ_TIMEOUT.as_secs(),
                            "Stream silence detected, no data received within timeout"
                        );
                        let _ = tx.send(Some(StreamEvent::Error(
                            rollball_core::providers::StreamError::stream_timeout(
                                STREAM_READ_TIMEOUT.as_secs(),
                            ),
                        ))).await;
                        return;
                    }
                }
            }
            let _ = tx.send(None).await;
        });

        Ok(Box::new(ChannelStream { rx }))
    }

    async fn chat_token_count(
        &self,
        messages: &[ChatMessage],
    ) -> rollball_core::error::Result<u64> {
        // Heuristic estimation for Anthropic models:
        // Claude models use a similar tokenization to GPT models.
        // Approximate: English ~4 chars/token, CJK ~2 chars/token
        let mut total_chars: usize = 0;
        let mut cjk_chars: usize = 0;

        for msg in messages {
            total_chars += msg.content.len();
            cjk_chars += msg
                .content
                .chars()
                .filter(|c| !c.is_ascii())
                .count();
        }

        let ascii_chars = total_chars - cjk_chars;
        let estimated_tokens =
            (ascii_chars as f64 / 4.0).ceil() as u64 + (cjk_chars as f64 / 2.0).ceil() as u64;

        Ok(estimated_tokens)
    }
}

// ── Streaming helpers ────────────────────────────────────────────────────

/// Channel-based stream for SSE events
struct ChannelStream {
    rx: mpsc::Receiver<Option<StreamEvent>>,
}

impl Stream for ChannelStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(Some(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(None)) => Poll::Ready(None),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Parse a single Anthropic SSE line into a StreamEvent.
///
/// `block_index_map` maps Anthropic's global content_block index to a
/// tool-call-only sequential index (0, 1, 2…). This is needed because
/// Anthropic assigns index to *all* content blocks (text + tool_use),
/// while the downstream loop expects indices that align with the tool_calls
/// array (i.e. only tool_use blocks counted). Without this mapping, if a
/// text block appears before tool_use blocks, the indices would be off-by-one
/// or more, causing argument chunks to be routed to wrong buffers.
fn parse_anthropic_sse_line(
    line: &str,
    pending_tool_id: &mut Option<String>,
    pending_tool_name: &mut Option<String>,
    accumulated_input_tokens: &mut u64,
    block_index_map: &mut HashMap<u64, u64>,
) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line == ":" {
        return None;
    }

    // Anthropic SSE format: "event: message_start" / "data: {...}"
    // We only process "data:" lines
    let data = line.strip_prefix("data: ")?;

    if data.is_empty() {
        return None;
    }

    let event: StreamEventRaw = serde_json::from_str(data).ok()?;

    match event.event_type.as_str() {
        "content_block_start" => {
            if let Some(block) = event.content_block
                && block.block_type.as_deref() == Some("tool_use")
            {
                let id = block.id.unwrap_or_default();
                let name = block.name.unwrap_or_default();
                *pending_tool_id = Some(id.clone());
                *pending_tool_name = Some(name.clone());
                // Map Anthropic's global content_block index to sequential tool-call index
                let tool_index = block_index_map.len() as u64;
                if let Some(content_block_index) = event.index {
                    block_index_map.insert(content_block_index, tool_index);
                }
                return Some(StreamEvent::ToolCallStart(ToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: String::new(),
                    },
                }));
            }
            None
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                if let Some(text) = delta.text
                    && !text.is_empty()
                {
                    return Some(StreamEvent::Content(text));
                }
                if let Some(partial_json) = delta.partial_json
                    && !partial_json.is_empty()
                {
                    // Route argument chunks to the correct tool_call buffer by content block index.
                    // Anthropic assigns each content block a unique global index (text blocks
                    // also occupy indices), but loop_.rs expects sequential tool-call indices
                    // (0, 1, 2…). We translate via block_index_map so downstream stays aligned.
                    let content_block_index = event.index.unwrap_or(0);
                    let tool_index = block_index_map.get(&content_block_index).copied().unwrap_or(0);
                    return Some(StreamEvent::ToolCallChunk { index: tool_index, arguments: partial_json });
                }
            }
            None
        }
        "message_stop" => {
            // Stream complete — will be handled by channel close (None)
            None
        }
        "message_delta" => {
            // Contains stop_reason and output usage info.
            // Combine accumulated input_tokens from message_start with output_tokens
            // from message_delta to produce a complete usage report.
            if let Some(usage) = event.usage {
                let input = *accumulated_input_tokens;
                let output = usage.output_tokens.unwrap_or(0);
                return Some(StreamEvent::Finished(ChatResponse {
                    content: String::new(),
                    tool_calls: None,
                    usage: Some(rollball_core::providers::traits::UsageInfo {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                    }),
                    ..Default::default()
                }));
            }
            None
        }
        "message_start" => {
            // Extract input_tokens from message_start's usage for later combination
            // with output_tokens from message_delta.
            if let Some(ref msg) = event.message
                && let Some(ref usage) = msg.usage
            {
                *accumulated_input_tokens = usage.input_tokens.unwrap_or(0);
            }
            None
        }
        "content_block_stop" | "ping" => {
            // No action needed for these event types
            None
        }
        "error" => {
            Some(StreamEvent::Error(
                rollball_core::providers::classify_stream_error(
                    &format!("Anthropic stream error: {data}"),
                ),
            ))
        }
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = AnthropicProvider::new(Some("sk-ant-test"));
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");

        let custom = AnthropicProvider::with_base_url(
            Some("https://custom-api.example.com"),
            Some("sk-ant-test"),
        );
        assert_eq!(custom.base_url, "https://custom-api.example.com");
    }

    #[test]
    fn test_set_api_key() {
        let mut provider = AnthropicProvider::new(None);
        assert!(provider.api_key.is_none());
        provider.set_api_key("sk-ant-new".to_string());
        assert_eq!(provider.api_key.as_deref(), Some("sk-ant-new"));
    }

    #[test]
    fn test_convert_messages_basic() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];

        let (converted, system) = convert_messages(&messages);
        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(converted.len(), 2); // system excluded from messages
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
    }

    #[test]
    fn test_convert_messages_with_tool_calls() {
        let messages = vec![ChatMessage::assistant_with_tools("", vec![ToolCall {
            id: "toolu_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Shanghai"}"#.to_string(),
            },
        }])];

        let (converted, _system) = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        let msg = &converted[0];
        let content = msg.content.as_ref().unwrap();
        let blocks = content.as_array().unwrap();
        // Should have one tool_use block
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
    }

    #[test]
    fn test_convert_messages_tool_result() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: r#"{"tool_call_id":"toolu_123","content":"Sunny, 25°C"}"#.to_string(),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (converted, _system) = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        let msg = &converted[0];
        assert_eq!(msg.role, "user"); // Anthropic requires tool_result in user role
        let content = msg.content.as_ref().unwrap();
        let blocks = content.as_array().unwrap();
        assert!(blocks[0]["type"] == "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_123");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![serde_json::json!({
            "name": "calculator",
            "description": "Performs calculations",
            "parameters": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string"}
                }
            }
        })];

        let converted = convert_tools(Some(&tools));
        assert!(converted.is_some());
        let tools = converted.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "calculator");
        assert_eq!(tools[0].description, "Performs calculations");
    }

    #[test]
    fn test_convert_tools_none() {
        let converted = convert_tools(None);
        assert!(converted.is_none());
    }

    #[test]
    fn test_parse_response_text_only() {
        let resp = AnthropicResponse {
            content: vec![AnthropicContentBlock {
                block_type: "text".to_string(),
                text: Some("Hello! How can I help?".to_string()),
                id: None,
                name: None,
                input: None,
            }],
            usage: Some(AnthropicUsage {
                input_tokens: Some(10),
                output_tokens: Some(8),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            stop_reason: Some("end_turn".to_string()),
        };

        let chat_resp = parse_response(resp);
        assert_eq!(chat_resp.content, "Hello! How can I help?");
        assert!(chat_resp.tool_calls.is_none());
        assert_eq!(chat_resp.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(chat_resp.usage.as_ref().unwrap().completion_tokens, 8);
        assert_eq!(chat_resp.usage.as_ref().unwrap().total_tokens, 18);
    }

    #[test]
    fn test_parse_response_with_tool_use() {
        let resp = AnthropicResponse {
            content: vec![
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: Some("Let me check that.".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "tool_use".to_string(),
                    text: None,
                    id: Some("toolu_abc".to_string()),
                    name: Some("weather".to_string()),
                    input: Some(serde_json::json!({"city": "Shanghai"})),
                },
            ],
            usage: Some(AnthropicUsage {
                input_tokens: Some(20),
                output_tokens: Some(15),
                cache_creation_input_tokens: Some(5),
                cache_read_input_tokens: None,
            }),
            stop_reason: Some("tool_use".to_string()),
        };

        let chat_resp = parse_response(resp);
        assert_eq!(chat_resp.content, "Let me check that.");
        assert!(chat_resp.tool_calls.is_some());
        let tool_calls = chat_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_abc");
        assert_eq!(tool_calls[0].function.name, "weather");
        assert!(tool_calls[0].function.arguments.contains("Shanghai"));

        // Cache creation tokens should be included in prompt_tokens
        assert_eq!(chat_resp.usage.as_ref().unwrap().prompt_tokens, 25); // 20 + 5
        assert_eq!(chat_resp.usage.as_ref().unwrap().completion_tokens, 15);
    }

    #[test]
    fn test_parse_sse_content_delta() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::Content(text)) = event {
            assert_eq!(text, "Hello");
        } else {
            panic!("Expected Content event");
        }
    }

    #[test]
    fn test_parse_sse_tool_use_start() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xyz","name":"calculator","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::ToolCallStart(tc)) = event {
            assert_eq!(tc.id, "toolu_xyz");
            assert_eq!(tc.function.name, "calculator");
        } else {
            panic!("Expected ToolCallStart event");
        }
        // content_block index 1 should map to tool-call index 0
        assert_eq!(block_index_map.get(&1), Some(&0u64));
    }

    #[test]
    fn test_parse_sse_tool_input_delta() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        // First register the tool_use at content_block index 1 -> tool-call index 0
        let _ = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xyz","name":"calculator","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        // Now send a delta for content_block index 1 — should be mapped to tool-call index 0
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"expr\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::ToolCallChunk { index, arguments: chunk }) = event {
            assert_eq!(index, 0, "index should be mapped to tool-call sequential index 0, not content_block index 1");
            assert!(chunk.contains("expr"));
        } else {
            panic!("Expected ToolCallChunk event");
        }
    }

    #[test]
    fn test_parse_sse_multi_tool_call_index_routing() {
        // Simulate Anthropic streaming where a text block precedes tool_use blocks.
        // content_block index 0 = text (skipped), index 1 = first tool, index 2 = second tool.
        // The block_index_map should translate: 1->0, 2->1 so downstream gets
        // sequential tool-call indices (0, 1) matching the tool_calls array.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        // content_block_start for first tool (content_block index 1, e.g. after text block at index 0)
        let e1 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_001","name":"shell","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        assert!(matches!(e1, Some(StreamEvent::ToolCallStart(ref tc)) if tc.id == "toolu_001" && tc.function.name == "shell"));

        // content_block_start for second tool (content_block index 2)
        let e2 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_002","name":"read_file","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        assert!(matches!(e2, Some(StreamEvent::ToolCallStart(ref tc)) if tc.id == "toolu_002" && tc.function.name == "read_file"));

        // Delta for first tool (content_block index 1) — should map to tool-call index 0
        let e3 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        if let Some(StreamEvent::ToolCallChunk { index, arguments }) = e3 {
            assert_eq!(index, 0, "first tool delta must have sequential tool-call index 0 (content_block 1 -> tool 0)");
            assert!(arguments.contains("command"));
        } else {
            panic!("Expected ToolCallChunk for index 0");
        }

        // Delta for second tool (content_block index 2) — should map to tool-call index 1
        let e4 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut block_index_map,
        );
        if let Some(StreamEvent::ToolCallChunk { index, arguments }) = e4 {
            assert_eq!(index, 1, "second tool delta must have sequential tool-call index 1 (content_block 2 -> tool 1)");
            assert!(arguments.contains("path"));
        } else {
            panic!("Expected ToolCallChunk for index 1");
        }
    }

    #[test]
    fn test_token_count_estimation() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let provider = AnthropicProvider::new(Some("test-key"));

        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "Hello world".to_string(),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let count = rt.block_on(provider.chat_token_count(&messages)).unwrap();
        assert!(count > 0);
        // "Hello world" = 11 chars / 4 ≈ 3 tokens
        assert!((2..=5).contains(&count));
    }

    #[test]
    fn test_token_count_cjk() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let provider = AnthropicProvider::new(Some("test-key"));

        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "你好世界".to_string(),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let count = rt.block_on(provider.chat_token_count(&messages)).unwrap();
        // 4 CJK chars ≈ 4/2 = 2 tokens
        assert!(count >= 2);
    }
}
