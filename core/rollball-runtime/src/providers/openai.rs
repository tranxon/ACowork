//! OpenAI Compatible Provider
//!
//! Supports OpenAI API and compatible endpoints (e.g., Azure OpenAI,
//! Together AI, Groq, DeepSeek, etc.) via configurable base_url.
//!
//! Adapted from zeroclaw/src/providers/openai.rs
//! Rollball deviation: uses rollball-core Provider trait instead of ZeroClaw's;
//! streaming uses futures_core::Stream instead of custom async stream.

use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

/// Per-chunk read timeout: if no data arrives within this duration,
/// the stream is considered stalled and we emit a StreamTimeout error.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(45);

use rollball_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, FunctionCall,
    MessageRole, Provider, StreamEvent, ToolCall, UsageInfo,
};

// ── Provider struct ──────────────────────────────────────────────────────

/// OpenAI-compatible provider
pub struct OpenAIProvider {
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider with default base URL
    pub fn new(api_key: Option<&str>) -> Self {
        Self::with_base_url(None, api_key)
    }

    /// Create a provider with a custom base URL
    pub fn with_base_url(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(600)) // 10 min: LLM streaming can be long (thinking + generation)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            http_client,
        }
    }

    /// Set API key after construction (e.g., from Vault KeyRelease)
    pub fn set_api_key(&mut self, key: String) {
        self.api_key = Some(key);
    }
}

// ── OpenAI API types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Request usage stats in the final streaming chunk (OpenAI stream_options)
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

/// OpenAI stream_options to request usage in the final chunk
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
struct NativeMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<NativeToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: NativeToolFunctionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeToolFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeToolCall {
    id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    function: NativeFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    choices: Vec<NativeChoice>,
    #[serde(default)]
    usage: Option<NativeUsage>,
}

#[derive(Debug, Deserialize)]
struct NativeChoice {
    message: NativeResponseMessage,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NativeResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<NativeToolCall>>,
}

#[derive(Debug, Deserialize)]
struct NativeUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

// Streaming SSE types
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// Usage info included in the final chunk when stream_options.include_usage = true
    #[serde(default)]
    usage: Option<NativeUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamToolCallDelta {
    index: Option<u64>,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

// ── Conversion helpers ──

fn convert_messages(messages: &[ChatMessage]) -> Vec<NativeMessage> {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };

            // Handle tool messages — prefer dedicated tool_call_id field,
            // fall back to parsing from content JSON for backward compatibility
            if matches!(m.role, MessageRole::Tool) {
                let tool_call_id = m.tool_call_id.clone().or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&m.content)
                        .ok()
                        .and_then(|v| v.get("tool_call_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                });
                let content = if m.tool_call_id.is_some() {
                    // tool_call_id is a separate field — content is the actual result
                    Some(m.content.clone())
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                    // Legacy format: content JSON contains tool_call_id and content
                    Some(value.get("content").and_then(serde_json::Value::as_str).unwrap_or("").to_string())
                } else {
                    Some(m.content.clone())
                };
                return NativeMessage {
                    role: role.to_string(),
                    content,
                    reasoning_content: None,
                    tool_call_id,
                    tool_calls: None,
                };
            }

            // Handle assistant messages with tool_calls
            if matches!(m.role, MessageRole::Assistant)
                && let Some(ref tool_calls) = m.tool_calls {
                let native_calls: Vec<NativeToolCall> = tool_calls
                    .iter()
                    .map(|tc| NativeToolCall {
                        id: Some(tc.id.clone()),
                        kind: Some(tc.call_type.clone()),
                        function: NativeFunctionCall {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        },
                    })
                    .collect();
                return NativeMessage {
                    role: role.to_string(),
                    content: if m.content.is_empty() {
                        None
                    } else {
                        Some(m.content.clone())
                    },
                    reasoning_content: m.reasoning_content.clone(),
                    tool_call_id: None,
                    tool_calls: Some(native_calls),
                };
            }

            NativeMessage {
                role: role.to_string(),
                content: Some(m.content.clone()),
                reasoning_content: m.reasoning_content.clone(),
                tool_call_id: None,
                tool_calls: None,
            }
        })
        .collect()
}

fn convert_tools(tools: Option<&[serde_json::Value]>) -> Option<Vec<NativeToolSpec>> {
    tools.map(|items| {
        items
            .iter()
            .map(|tool| {
                let name = tool["name"].as_str().unwrap_or("unknown").to_string();
                tracing::debug!(
                    tool = %name,
                    has_parameters = tool.get("parameters").is_some(),
                    tool_keys = ?tool.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                    "OpenAI convert_tools field check"
                );
                let description = tool
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let parameters = match tool.get("parameters") {
                    Some(p) if p.is_object() => p.clone(),
                    Some(p) => {
                        tracing::warn!(
                            tool_name = %name,
                            parameters_type = ?p,
                            "Tool parameters is not a JSON object, using default schema"
                        );
                        serde_json::json!({"type": "object", "properties": {}})
                    }
                    None => {
                        tracing::warn!(
                            tool_name = %name,
                            "Tool definition missing 'parameters' field, using default schema"
                        );
                        serde_json::json!({"type": "object", "properties": {}})
                    }
                };

                NativeToolSpec {
                    kind: "function".to_string(),
                    function: NativeToolFunctionSpec {
                        name,
                        description,
                        parameters,
                    },
                }
            })
            .collect()
    })
}

fn parse_response(msg: NativeResponseMessage, usage: Option<NativeUsage>) -> ChatResponse {
    let content = msg.content.unwrap_or_default();
    let tool_calls = msg.tool_calls.unwrap_or_default().into_iter().map(|tc| {
        ToolCall {
            id: tc.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tc.function.name,
                arguments: tc.function.arguments,
            },
        }
    }).collect::<Vec<_>>();

    let usage_info = usage.map(|u| UsageInfo {
        prompt_tokens: u.prompt_tokens.unwrap_or(0),
        completion_tokens: u.completion_tokens.unwrap_or(0),
        total_tokens: u.prompt_tokens.unwrap_or(0) + u.completion_tokens.unwrap_or(0),
    });

    ChatResponse {
        content,
        reasoning_content: msg.reasoning_content,
        tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
        usage: usage_info,
        reasoning_started_at: None,
        reasoning_finished_at: None,
    }
}

// ── Provider trait implementation ───────────────────────────────────────

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat(&self, request: ChatRequest) -> rollball_core::error::Result<ChatResponse> {
        let native_request = NativeChatRequest {
            model: request.model,
            messages: convert_messages(&request.messages),
            temperature: request.temperature.unwrap_or(0.7),
            max_tokens: request.max_tokens,
            tools: convert_tools(request.tools.as_deref()),
            stream: None,
            stream_options: None,
        };

        // Log request payload for debugging tool definitions
        tracing::debug!(
            request_len = serde_json::to_string(&native_request).map(|s| s.len()).unwrap_or(0),
            model = %native_request.model,
            has_tools = native_request.tools.is_some(),
            "OpenAI chat request"
        );

        let url = format!("{}/chat/completions", self.base_url);

        let mut req_builder = self.http_client.post(&url);

        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.bearer_auth(api_key);
        }

        let response = req_builder
            .json(&native_request)
            .send()
            .await
            .map_err(|e| rollball_core::RollballError::Provider(rollball_core::ProviderError::network(format!("OpenAI request failed: {e}"))))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            // Detailed diagnostics for 400 Bad Request errors
            if status.as_u16() == 400 {
                tracing::error!(
                    tools_count = native_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                    messages_count = native_request.messages.len(),
                    last_message_role = ?native_request.messages.last().map(|m| &m.role),
                    error_body = %body,
                    "LLM returned 400 Bad Request - detailed diagnostics"
                );
                if body.contains("invalid function arguments") {
                    // Log the last assistant message's tool_calls for diagnosis
                    if let Some(last_assistant) = native_request.messages.iter().rev()
                        .find(|m| m.role == "assistant")
                    {
                        tracing::error!(
                            last_assistant_tool_calls = ?last_assistant.tool_calls,
                            "Diagnosing invalid function arguments - last assistant tool_calls"
                        );
                    }
                }
            }

            return Err(rollball_core::RollballError::Provider(rollball_core::ProviderError::from_status_code(status.as_u16(), format!("OpenAI API error: {status} — {body}"))));
        }

        let native_resp: NativeChatResponse = response
            .json()
            .await
            .map_err(|e| rollball_core::RollballError::Provider(rollball_core::ProviderError::unknown(format!("Failed to parse OpenAI response: {e}"))))?;

        let choice = native_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| rollball_core::RollballError::Provider(rollball_core::ProviderError::unknown("No choices in OpenAI response".to_string())))?;

        Ok(parse_response(choice.message, native_resp.usage))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> rollball_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let native_request = NativeChatRequest {
            model: request.model,
            messages: convert_messages(&request.messages),
            temperature: request.temperature.unwrap_or(0.7),
            max_tokens: request.max_tokens,
            tools: convert_tools(request.tools.as_deref()),
            stream: Some(true),
            stream_options: Some(StreamOptions { include_usage: true }),
        };

        // Log request payload for debugging tool definitions
        tracing::info!(
            model = %native_request.model,
            has_tools = native_request.tools.is_some(),
            tool_count = native_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            messages_count = native_request.messages.len(),
            max_tokens = ?native_request.max_tokens,
            request_payload_size = serde_json::to_string(&native_request).map(|s| s.len()).unwrap_or(0),
            "OpenAI chat_stream request"
        );
        if let Some(ref tools) = native_request.tools {
            for tool in tools {
                tracing::info!(
                    tool_name = %tool.function.name,
                    has_parameters = !tool.function.parameters.is_null(),
                    param_keys = ?tool.function.parameters.get("properties").map(|p| p.as_object().map(|o| o.keys().collect::<Vec<_>>())),
                    "OpenAI request tool definition"
                );
            }
        }

        let url = format!("{}/chat/completions", self.base_url);

        let response = self.send_streaming_request(&url, &native_request).await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();

            // Fallback: if the error is caused by stream_options (some OpenAI-compatible
            // APIs don't support this field), retry without it.
            if (status.as_u16() == 422 || status.as_u16() == 400)
                && body.contains("stream_options")
            {
                tracing::warn!(
                    status = %status,
                    "stream_options not supported, retrying without it"
                );
                let fallback_request = NativeChatRequest {
                    model: native_request.model.clone(),
                    messages: native_request.messages.clone(),
                    temperature: native_request.temperature,
                    max_tokens: native_request.max_tokens,
                    tools: native_request.tools.clone(),
                    stream: Some(true),
                    stream_options: None,
                };
                let retry_response = self.send_streaming_request(&url, &fallback_request).await?;
                if !retry_response.status().is_success() {
                    let s = retry_response.status();
                    let b = retry_response.text().await.unwrap_or_default();
                    return Err(rollball_core::RollballError::Provider(
                        rollball_core::ProviderError::from_status_code(s.as_u16(), format!("OpenAI API error: {s} - {b}"))
                    ));
                }
                return Ok(Self::sse_to_stream(retry_response));
            }

            // Detailed diagnostics for 400 Bad Request errors
            if status.as_u16() == 400 {
                tracing::error!(
                    tools_count = native_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                    messages_count = native_request.messages.len(),
                    last_message_role = ?native_request.messages.last().map(|m| &m.role),
                    error_body = %body,
                    "LLM returned 400 Bad Request - detailed diagnostics"
                );
                if body.contains("invalid function arguments")
                    && let Some(last_assistant) = native_request.messages.iter().rev()
                        .find(|m| m.role == "assistant")
                {
                    tracing::error!(
                        last_assistant_tool_calls = ?last_assistant.tool_calls,
                        "Diagnosing invalid function arguments - last assistant tool_calls"
                    );
                }
            }

            return Err(rollball_core::RollballError::Provider(
                rollball_core::ProviderError::from_status_code(status.as_u16(), format!("OpenAI API error: {status} - {body}"))
            ));
        }

        Ok(Self::sse_to_stream(response))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> rollball_core::error::Result<u64> {
        // Approximate token count: ~4 chars per token for English text
        // This is a rough estimate; precise counting requires tiktoken
        let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        Ok((total_chars as f64 / 4.0).ceil() as u64)
    }
}

// ── OpenAIProvider inherent helpers (not part of Provider trait) ─────────

impl OpenAIProvider {
    /// Send a streaming HTTP request and return the raw response.
    async fn send_streaming_request(
        &self,
        url: &str,
        native_request: &NativeChatRequest,
    ) -> rollball_core::error::Result<reqwest::Response> {
        let mut req_builder = self.http_client.post(url);
        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.bearer_auth(api_key);
        }
        req_builder
            .json(native_request)
            .send()
            .await
            .map_err(|e| rollball_core::RollballError::Provider(
                rollball_core::ProviderError::network(format!("OpenAI streaming request failed: {e}"))
            ))
    }

    /// Convert an HTTP SSE response into a Stream of StreamEvent.
    fn sse_to_stream(response: reqwest::Response) -> Box<dyn Stream<Item = StreamEvent> + Send> {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();

            use futures_util::StreamExt;
            loop {
                match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer[..newline_pos].to_string();
                            buffer = buffer[newline_pos + 1..].to_string();

                            if line.trim() == "data: [DONE]" {
                                let _ = tx.send(None).await;
                                return;
                            }

                            for event in parse_sse_line(&line) {
                                if tx.send(Some(event)).await.is_err() {
                                    return; // receiver dropped
                                }
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

        Box::new(ChannelStream { rx })
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
            Poll::Ready(Some(None)) => Poll::Ready(None), // stream done
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Parse a single SSE line into one or more StreamEvents.
fn parse_sse_line(line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    tracing::debug!(
        line = %line.chars().take(500).collect::<String>(),
        "SSE raw line received"
    );
    if line.is_empty() || line == ":" {
        return Vec::new();
    }

    let Some(data) = line.strip_prefix("data: ") else {
        return Vec::new();
    };
    if data == "[DONE]" {
        return Vec::new();
    }

    let Some(chunk) = serde_json::from_str::<StreamChunk>(data).ok() else {
        return Vec::new();
    };

    // If the final streaming chunk includes usage info (requested via
    // stream_options.include_usage), emit a Finished event so the
    // agent loop can compute context usage. This is the standard
    // OpenAI behavior: the last chunk before [DONE] contains usage.
    if let Some(usage) = chunk.usage {
        let prompt = usage.prompt_tokens.unwrap_or(0);
        let completion = usage.completion_tokens.unwrap_or(0);
        return vec![StreamEvent::Finished(ChatResponse {
            content: String::new(),
            tool_calls: None,
            usage: Some(rollball_core::providers::traits::UsageInfo {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
            }),
            ..Default::default()
        })];
    }

    let mut events = Vec::new();

    for choice in chunk.choices {
        if let Some(ref content) = choice.delta.content
            && !content.is_empty()
        {
            events.push(StreamEvent::Content(content.clone()));
        }

        if let Some(ref rc) = choice.delta.reasoning_content
            && !rc.is_empty()
        {
            events.push(StreamEvent::ReasoningContent(rc.clone()));
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tc_delta in tool_calls {
                if let Some(func) = tc_delta.function {
                    if let Some(name) = func.name
                        && !name.is_empty()
                    {
                        // Capture arguments from the same delta chunk if present.
                        // Some providers (GLM, DeepSeek) send name and arguments
                        // together in a single SSE chunk; previously the early
                        // return discarded arguments, causing empty tool params.
                        let initial_arguments = func.arguments.unwrap_or_default();

                        tracing::debug!(
                            has_name = true,
                            name = %name,
                            has_arguments = !initial_arguments.is_empty(),
                            args_preview = ?initial_arguments.chars().take(200).collect::<String>(),
                            index = ?tc_delta.index,
                            "SSE tool_call delta details"
                        );
                        events.push(StreamEvent::ToolCallStart(ToolCall {
                            id: tc_delta
                                .id
                                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name,
                                arguments: initial_arguments,
                            },
                        }));
                    } else {
                        // DeepSeek sends empty name string in subsequent tool_call
                        // chunks; fall through to process arguments instead of
                        // skipping the entire delta.
                        tracing::debug!(
                            has_name = false,
                            has_arguments = func.arguments.is_some(),
                            args_preview = ?func.arguments.as_ref().map(|a| a.chars().take(200).collect::<String>()),
                            index = ?tc_delta.index,
                            "SSE tool_call delta details"
                        );
                        if let Some(args) = func.arguments {
                            let idx = tc_delta.index.unwrap_or(0);
                            events.push(StreamEvent::ToolCallChunk { index: idx, arguments: args });
                        }
                    }
                }
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_basic() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::System,
                content: "You are helpful.".to_string(),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: "Hello".to_string(),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let native = convert_messages(&messages);
        assert_eq!(native.len(), 2);
        assert_eq!(native[0].role, "system");
        assert_eq!(native[1].role, "user");
    }

    #[test]
    fn test_convert_messages_with_tool_calls() {
        let messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            content: "".to_string(),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "weather".to_string(),
                    arguments: "{\"city\":\"Shanghai\"}".to_string(),
                },
            }]),
        }];

        let native = convert_messages(&messages);
        assert_eq!(native[0].role, "assistant");
        assert!(native[0].tool_calls.is_some());
    }

    #[test]
    fn test_provider_creation() {
        let provider = OpenAIProvider::new(None);
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.base_url, "https://api.openai.com/v1");

        let custom = OpenAIProvider::with_base_url(
            Some("https://api.deepseek.com/v1"),
            Some("sk-test"),
        );
        assert_eq!(custom.base_url, "https://api.deepseek.com/v1");
    }

    #[test]
    fn test_parse_response() {
        let msg = NativeResponseMessage {
            content: Some("Hello!".to_string()),
            reasoning_content: None,
            tool_calls: None,
        };
        let resp = parse_response(msg, None);
        assert_eq!(resp.content, "Hello!");
        assert!(resp.tool_calls.is_none());

        let msg_with_tc = NativeResponseMessage {
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![NativeToolCall {
                id: Some("call_1".to_string()),
                kind: None,
                function: NativeFunctionCall {
                    name: "calculator".to_string(),
                    arguments: "{\"expr\":\"2+2\"}".to_string(),
                },
            }]),
        };
        let resp = parse_response(msg_with_tc, Some(NativeUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
        }));
        assert!(resp.tool_calls.is_some());
        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 15);
    }

    #[test]
    fn test_convert_tools_reads_parameters_field() {
        // Simulate ToolSpec serialized with #[serde(rename = "parameters")]
        let tool_json = serde_json::json!({
            "name": "shell",
            "description": "Execute shell commands",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }
        });

        let tools = vec![tool_json];
        let native = convert_tools(Some(&tools)).unwrap();

        assert_eq!(native.len(), 1);
        assert_eq!(native[0].kind, "function");
        assert_eq!(native[0].function.name, "shell");
        assert_eq!(native[0].function.description, "Execute shell commands");

        // Verify parameters were correctly extracted
        let params = &native[0].function.parameters;
        assert!(params.get("properties").is_some());
        assert!(params.get("properties").unwrap().get("command").is_some());

        // Verify the serialized NativeToolSpec has correct structure
        let serialized = serde_json::to_value(&native[0]).unwrap();
        assert_eq!(serialized.get("type").unwrap(), "function");
        let function = serialized.get("function").unwrap();
        assert!(function.get("parameters").is_some());
        assert!(function.get("name").is_some());

        println!("Serialized NativeToolSpec: {}", serde_json::to_string_pretty(&serialized).unwrap());
    }

    #[test]
    fn test_convert_tools_fallback_when_parameters_missing() {
        // Tool JSON without parameters field — should fallback to empty object
        let tool_json = serde_json::json!({
            "name": "no_params_tool",
            "description": "A tool without parameters"
        });

        let tools = vec![tool_json];
        let native = convert_tools(Some(&tools)).unwrap();

        assert_eq!(native.len(), 1);
        assert_eq!(native[0].function.name, "no_params_tool");
        // Should fallback to empty object schema
        assert_eq!(
            native[0].function.parameters,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }
}
