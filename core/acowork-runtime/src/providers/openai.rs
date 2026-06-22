//! OpenAI Compatible Provider
//!
//! Supports OpenAI API and compatible endpoints (e.g., Azure OpenAI,
//! Together AI, Groq, DeepSeek, etc.) via configurable base_url.
//!
//! Adapted from zeroclaw/src/providers/openai.rs
//! AgentCowork deviation: uses acowork-core Provider trait instead of ZeroClaw's;
//! streaming uses futures_core::Stream instead of custom async stream.
//! SPDX-License-Identifier: MIT OR Apache-2.0

use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, MessageRole, Provider,
    ReasoningEffort, StreamEvent, ToolCall, UsageInfo,
};
use acowork_core::tools::schema::sanitize_tool_schema;

/// Default per-chunk read timeout (45s) — used by backwards-compatible constructors.
const DEFAULT_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(45);

// ── Provider struct ──────────────────────────────────────────────────────

/// OpenAI-compatible provider
pub struct OpenAIProvider {
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
    stream_read_timeout: Duration,
}

impl OpenAIProvider {
    /// Create a new OpenAI provider with default base URL and default timeouts
    pub fn new(api_key: Option<&str>) -> Self {
        Self::with_base_url(None, api_key)
    }

    /// Create a provider with a custom base URL and default timeouts
    pub fn with_base_url(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        Self::with_base_url_and_timeouts(
            base_url,
            api_key,
            Duration::from_secs(600),
            Duration::from_secs(10),
            DEFAULT_STREAM_READ_TIMEOUT,
        )
    }

    /// Create a provider with fully configurable timeouts
    pub fn with_base_url_and_timeouts(
        base_url: Option<&str>,
        api_key: Option<&str>,
        request_timeout: Duration,
        connect_timeout: Duration,
        stream_read_timeout: Duration,
    ) -> Self {
        let http_client = Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            http_client,
            stream_read_timeout,
        }
    }

    /// Set API key after construction (e.g., from Vault KeyRelease)
    pub fn set_api_key(&mut self, key: String) {
        self.api_key = Some(key);
    }
}

/// Map a [`ReasoningEffort`] to the OpenAI `reasoning_effort` field value.
///
/// OpenAI's `reasoning_effort` parameter only accepts "low" | "medium" | "high"
/// (some models also accept "minimal"). It has **no explicit "disable"** value —
/// omitting the field lets the model use its own default, which is typically
/// "medium" for o-series models. To honor the user's intent of "minimum
/// reasoning", we map `Off` to `"low"` (the lowest universally-supported value).
/// `Auto` means the user wants the model to decide; the field is also omitted.
fn openai_reasoning_str(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        // Auto: let the model decide, do not send the field.
        ReasoningEffort::Auto => None,
        // No disable protocol — collapse to the lowest available effort.
        ReasoningEffort::Off => Some("low"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        // Most OpenAI-compatible APIs cap at "high".
        ReasoningEffort::Max => Some("high"),
    }
}

// ── OpenAI API types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Request usage stats in the final streaming chunk (OpenAI stream_options)
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// Reasoning effort for thinking-capable models (OpenAI o-series, MiniMax, etc.)
    /// Maps to `reasoning_effort` in the OpenAI-compatible API.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

/// OpenAI stream_options to request usage in the final chunk
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
struct NativeMessage {
    role: String,
    /// Content — either a plain String or a multimodal JSON array of content parts.
    /// Uses serde_json::Value for flexible serialization:
    /// - String for plain text messages
    /// - Array of content part objects for multimodal messages
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
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
    #[serde(default)]
    prompt_tokens_details: Option<NativePromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<NativeCompletionTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct NativePromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct NativeCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
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
    /// OpenAI protocol: "stop" | "length" | "tool_calls" | null (intermediate chunks)
    #[serde(default)]
    finish_reason: Option<String>,
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

/// Convert a ChatMessage's content to the appropriate serde_json::Value.
///
/// When `content_parts` is present, produces a multimodal content array
/// (e.g. `[{"type":"text","text":"..."}, {"type":"image_url","image_url":{...}}]`).
/// Otherwise falls back to a plain String value from `content`.
fn build_content_value(m: &ChatMessage) -> serde_json::Value {
    if let Some(ref parts) = m.content_parts {
        let arr: Vec<serde_json::Value> = parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => {
                    serde_json::json!({ "type": "text", "text": text })
                }
                ContentPart::ImageUrl { image_url } => {
                    let mut img = serde_json::json!({ "url": image_url.url });
                    if let Some(ref detail) = image_url.detail {
                        img["detail"] = serde_json::json!(detail);
                    }
                    serde_json::json!({ "type": "image_url", "image_url": img })
                }
            })
            .collect();
        serde_json::Value::Array(arr)
    } else {
        serde_json::Value::String(m.content.clone())
    }
}

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
                // Tool messages never carry multimodal parts — use plain content
                let tool_call_id = m.tool_call_id.clone().or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&m.content)
                        .ok()
                        .and_then(|v| {
                            v.get("tool_call_id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                });
                let content = if m.tool_call_id.is_some() {
                    // tool_call_id is a separate field — content is the actual result
                    Some(serde_json::Value::String(m.content.clone()))
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content) {
                    // Legacy format: content JSON contains tool_call_id and content
                    Some(serde_json::Value::String(
                        value
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ))
                } else {
                    Some(serde_json::Value::String(m.content.clone()))
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
                && let Some(ref tool_calls) = m.tool_calls
            {
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
                    content: if m.content.is_empty() && m.content_parts.is_none() {
                        None
                    } else {
                        Some(build_content_value(m))
                    },
                    reasoning_content: m.reasoning_content.clone(),
                    tool_call_id: None,
                    tool_calls: Some(native_calls),
                };
            }

            NativeMessage {
                role: role.to_string(),
                content: Some(build_content_value(m)),
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
                    Some(p) if p.is_object() => sanitize_tool_schema(p),
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
    let tool_calls = msg
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tc.function.name,
                arguments: tc.function.arguments,
            },
        })
        .collect::<Vec<_>>();

    let usage_info = usage.map(|u| {
        let prompt = u.prompt_tokens.unwrap_or(0);
        let completion = u.completion_tokens.unwrap_or(0);
        let cache_read = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = u
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);
        UsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0, // OpenAI protocol has no cache write field
            reasoning_tokens: reasoning,
        }
    });

    ChatResponse {
        content,
        reasoning_content: msg.reasoning_content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: usage_info,
        reasoning_started_at: None,
        reasoning_finished_at: None,
        finish_reason: None,
    }
}

// ── Provider trait implementation ───────────────────────────────────────

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        let reasoning = request
            .reasoning_effort
            .as_ref()
            .and_then(|e| openai_reasoning_str(e))
            .map(|s| s.to_string());
        let native_request = NativeChatRequest {
            model: request.model,
            messages: convert_messages(&request.messages),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            tools: convert_tools(request.tools.as_deref()),
            stream: None,
            stream_options: None,
            reasoning_effort: reasoning,
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
            .map_err(|e| {
                acowork_core::AcoworkError::Provider(acowork_core::ProviderError::network(format!(
                    "OpenAI request failed: {e}"
                )))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::providers::parse_retry_after_header(response.headers());
            let body = response.text().await.unwrap_or_default();

            // Fallback: if the error is 400/422 and reasoning_effort is present,
            // retry without it. Many OpenAI-compatible providers reject this
            // non-standard field.
            if (status.as_u16() == 400 || status.as_u16() == 422)
                && native_request.reasoning_effort.is_some()
            {
                tracing::warn!(
                    status = %status,
                    "reasoning_effort not supported in non-streaming chat, retrying without it"
                );
                let fallback_request = NativeChatRequest {
                    model: native_request.model.clone(),
                    messages: native_request.messages.clone(),
                    temperature: native_request.temperature,
                    max_tokens: native_request.max_tokens,
                    tools: native_request.tools.clone(),
                    stream: None,
                    stream_options: None,
                    reasoning_effort: None,
                };
                let fallback_response = {
                    let mut fb_builder = self.http_client.post(&url);
                    if let Some(ref api_key) = self.api_key {
                        fb_builder = fb_builder.bearer_auth(api_key);
                    }
                    fb_builder
                        .json(&fallback_request)
                        .send()
                        .await
                        .map_err(|e| {
                            acowork_core::AcoworkError::Provider(
                                acowork_core::ProviderError::network(format!(
                                    "OpenAI request failed: {e}"
                                )),
                            )
                        })?
                };

                if fallback_response.status().is_success() {
                    let native_resp: NativeChatResponse = fallback_response
                        .json()
                        .await
                        .map_err(|e| {
                            acowork_core::AcoworkError::Provider(
                                acowork_core::ProviderError::unknown(format!(
                                    "Failed to parse OpenAI response: {e}"
                                )),
                            )
                        })?;
                    let choice = native_resp.choices.into_iter().next().ok_or_else(|| {
                        acowork_core::AcoworkError::Provider(
                            acowork_core::ProviderError::unknown(
                                "No choices in OpenAI response".to_string(),
                            ),
                        )
                    })?;
                    return Ok(parse_response(choice.message, native_resp.usage));
                }
                // Fallback also failed — fall through to error with the fallback response
                let f_status = fallback_response.status();
                let f_body = fallback_response.text().await.unwrap_or_default();
                let mut err = acowork_core::ProviderError::from_status_code(
                    f_status.as_u16(),
                    format!("OpenAI API error: {f_status} — {f_body}"),
                );
                err.retry_after_ms = retry_after;
                return Err(acowork_core::AcoworkError::Provider(err));
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
                if body.contains("invalid function arguments") {
                    // Log the last assistant message's tool_calls for diagnosis
                    if let Some(last_assistant) = native_request
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == "assistant")
                    {
                        tracing::error!(
                            last_assistant_tool_calls = ?last_assistant.tool_calls,
                            "Diagnosing invalid function arguments - last assistant tool_calls"
                        );
                    }
                }
            }

            let mut err = acowork_core::ProviderError::from_status_code(
                status.as_u16(),
                format!("OpenAI API error: {status} — {body}"),
            );
            err.retry_after_ms = retry_after;
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        let native_resp: NativeChatResponse = response.json().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unknown(format!(
                "Failed to parse OpenAI response: {e}"
            )))
        })?;

        let choice = native_resp.choices.into_iter().next().ok_or_else(|| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unknown(
                "No choices in OpenAI response".to_string(),
            ))
        })?;

        Ok(parse_response(choice.message, native_resp.usage))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> acowork_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let reasoning = request
            .reasoning_effort
            .as_ref()
            .and_then(|e| openai_reasoning_str(e))
            .map(|s| s.to_string());
        let native_request = NativeChatRequest {
            model: request.model,
            messages: convert_messages(&request.messages),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            tools: convert_tools(request.tools.as_deref()),
            stream: Some(true),
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            reasoning_effort: reasoning,
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
            let retry_after = crate::providers::parse_retry_after_header(response.headers());
            let body = response.text().await.unwrap_or_default();

            // Progressive fallback chain for 400/422 errors.
            // Many OpenAI-compatible providers reject various non-standard
            // fields. We progressively strip fields and retry.
            if status.as_u16() == 422 || status.as_u16() == 400 {
                // Log the full request JSON for diagnosis
                let request_json = serde_json::to_string(&native_request)
                    .unwrap_or_else(|_| "<serialization failed>".to_string());
                tracing::warn!(
                    status = %status,
                    error_body = %body,
                    request_json = %request_json,
                    "Initial 400/422 — starting progressive fallback"
                );

                // Fallback 1: strip stream_options only
                tracing::warn!("Fallback 1/3: stripping stream_options");
                let fb1 = NativeChatRequest {
                    model: native_request.model.clone(),
                    messages: native_request.messages.clone(),
                    temperature: native_request.temperature,
                    max_tokens: native_request.max_tokens,
                    tools: native_request.tools.clone(),
                    stream: Some(true),
                    stream_options: None,
                    reasoning_effort: native_request.reasoning_effort.clone(),
                };
                let resp1 = self.send_streaming_request(&url, &fb1).await?;
                if resp1.status().is_success() {
                    return Ok(Self::sse_to_stream(resp1, self.stream_read_timeout));
                }
                let s1 = resp1.status();
                let b1 = resp1.text().await.unwrap_or_default();
                tracing::warn!(status = %s1, error_body = %b1, "Fallback 1 failed");

                // Fallback 2: also strip reasoning_effort
                if native_request.reasoning_effort.is_some() {
                    tracing::warn!("Fallback 2/3: also stripping reasoning_effort");
                    let fb2 = NativeChatRequest {
                        model: native_request.model.clone(),
                        messages: native_request.messages.clone(),
                        temperature: native_request.temperature,
                        max_tokens: native_request.max_tokens,
                        tools: native_request.tools.clone(),
                        stream: Some(true),
                        stream_options: None,
                        reasoning_effort: None,
                    };
                    let resp2 = self.send_streaming_request(&url, &fb2).await?;
                    if resp2.status().is_success() {
                        return Ok(Self::sse_to_stream(resp2, self.stream_read_timeout));
                    }
                    let s2 = resp2.status();
                    let b2 = resp2.text().await.unwrap_or_default();
                    tracing::warn!(status = %s2, error_body = %b2, "Fallback 2 failed");
                }

                // Fallback 3: also strip tools (last resort — model gets no tools)
                tracing::warn!("Fallback 3/3: also stripping tools (last resort)");
                let fb3 = NativeChatRequest {
                    model: native_request.model.clone(),
                    messages: native_request.messages.clone(),
                    temperature: native_request.temperature,
                    max_tokens: native_request.max_tokens,
                    tools: None,
                    stream: Some(true),
                    stream_options: None,
                    reasoning_effort: None,
                };
                let resp3 = self.send_streaming_request(&url, &fb3).await?;
                if resp3.status().is_success() {
                    tracing::warn!(
                        "Fallback 3 succeeded — the 400 was caused by tool definitions. \
                         Tools have been stripped for this request."
                    );
                    return Ok(Self::sse_to_stream(resp3, self.stream_read_timeout));
                }
                let s3 = resp3.status();
                let b3 = resp3.text().await.unwrap_or_default();

                // All fallbacks failed — log comprehensive diagnostics
                tracing::error!(
                    model = %native_request.model,
                    tools_count = native_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                    messages_count = native_request.messages.len(),
                    has_stream_options = native_request.stream_options.is_some(),
                    has_reasoning_effort = native_request.reasoning_effort.is_some(),
                    has_temperature = native_request.temperature.is_some(),
                    max_tokens = ?native_request.max_tokens,
                    initial_error = %body,
                    fb1_error = %b1,
                    fb3_error = %b3,
                    request_json = %request_json,
                    "All streaming fallbacks failed — full diagnostics"
                );

                let mut err = acowork_core::ProviderError::from_status_code(
                    s3.as_u16(),
                    format!("OpenAI API error: {s3} - {b3}"),
                );
                err.retry_after_ms = retry_after;
                return Err(acowork_core::AcoworkError::Provider(err));
            }

            // Detailed diagnostics for other 400 Bad Request errors
            if status.as_u16() == 400 {
                tracing::error!(
                    tools_count = native_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
                    messages_count = native_request.messages.len(),
                    last_message_role = ?native_request.messages.last().map(|m| &m.role),
                    error_body = %body,
                    "LLM returned 400 Bad Request - detailed diagnostics"
                );
                if body.contains("invalid function arguments")
                    && let Some(last_assistant) = native_request
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == "assistant")
                {
                    tracing::error!(
                        last_assistant_tool_calls = ?last_assistant.tool_calls,
                        "Diagnosing invalid function arguments - last assistant tool_calls"
                    );
                }
            }

            let mut err = acowork_core::ProviderError::from_status_code(
                status.as_u16(),
                format!("OpenAI API error: {status} - {body}"),
            );
            err.retry_after_ms = retry_after;
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        Ok(Self::sse_to_stream(response, self.stream_read_timeout))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> acowork_core::error::Result<u64> {
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
    ) -> acowork_core::error::Result<reqwest::Response> {
        let mut req_builder = self.http_client.post(url);
        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.bearer_auth(api_key);
        }
        req_builder.json(native_request).send().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::network(format!(
                "OpenAI streaming request failed: {e}"
            )))
        })
    }

    /// Convert an HTTP SSE response into a Stream of StreamEvent.
    fn sse_to_stream(
        response: reqwest::Response,
        stream_read_timeout: Duration,
    ) -> Box<dyn Stream<Item = StreamEvent> + Send> {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            // Track usage and finish_reason across SSE chunks so we can
            // emit a single Finished event at stream end with both.
            let mut tracked_usage: Option<UsageInfo> = None;
            let mut tracked_finish_reason: Option<String> = None;

            use futures_util::StreamExt;
            loop {
                match tokio::time::timeout(stream_read_timeout, stream.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer[..newline_pos].to_string();
                            buffer = buffer[newline_pos + 1..].to_string();

                            if line.trim() == "data: [DONE]" {
                                // Emit final Finished event with accumulated data
                                let _ = tx
                                    .send(Some(StreamEvent::Finished(ChatResponse {
                                        content: String::new(),
                                        tool_calls: None,
                                        usage: tracked_usage,
                                        finish_reason: tracked_finish_reason,
                                        ..Default::default()
                                    })))
                                    .await;
                                let _ = tx.send(None).await;
                                return;
                            }

                            let (events, usage, finish_reason) = parse_sse_line(&line);
                            if usage.is_some() {
                                tracked_usage = usage;
                            }
                            if finish_reason.is_some() {
                                tracked_finish_reason = finish_reason;
                            }
                            for event in events {
                                if tx.send(Some(event)).await.is_err() {
                                    return; // receiver dropped
                                }
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        let stream_err = acowork_core::providers::classify_stream_error(&format!(
                            "Stream error: {e}"
                        ));
                        let _ = tx.send(Some(StreamEvent::Error(stream_err))).await;
                        return;
                    }
                    Ok(None) => {
                        // Stream ended normally (no [DONE] marker)
                        break;
                    }
                    Err(_) => {
                        // Stream silence — no data received within read timeout
                        tracing::warn!(
                            timeout_secs = stream_read_timeout.as_secs(),
                            "Stream silence detected, no data received within timeout"
                        );
                        let _ = tx
                            .send(Some(StreamEvent::Error(
                                acowork_core::providers::StreamError::stream_timeout(
                                    stream_read_timeout.as_secs(),
                                ),
                            )))
                            .await;
                        return;
                    }
                }
            }
            // Stream ended without [DONE] — emit Finished with accumulated data
            // (common with OpenAI-compatible APIs like MiniMax).
            let _ = tx
                .send(Some(StreamEvent::Finished(ChatResponse {
                    content: String::new(),
                    tool_calls: None,
                    usage: tracked_usage,
                    finish_reason: tracked_finish_reason,
                    ..Default::default()
                })))
                .await;
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

/// Parse a single SSE line into stream events, optional usage data, and optional finish_reason.
///
/// Returns `(events, usage, finish_reason)`:
/// - `events`: Content, ReasoningContent, ToolCallStart, ToolCallChunk events
/// - `usage`: extracted usage info from the final usage chunk (if present)
/// - `finish_reason`: from the last choice with a non-null finish_reason
///
/// The caller (sse_to_stream) is responsible for emitting the `Finished` event
/// at stream end, combining usage + finish_reason into a single ChatResponse.
fn parse_sse_line(line: &str) -> (Vec<StreamEvent>, Option<UsageInfo>, Option<String>) {
    let line = line.trim();
    tracing::debug!(
        line = %line.chars().take(500).collect::<String>(),
        "SSE raw line received"
    );
    if line.is_empty() || line == ":" {
        return (Vec::new(), None, None);
    }

    let Some(data) = line.strip_prefix("data: ") else {
        return (Vec::new(), None, None);
    };
    if data == "[DONE]" {
        return (Vec::new(), None, None);
    }

    let Some(chunk) = serde_json::from_str::<StreamChunk>(data).ok() else {
        return (Vec::new(), None, None);
    };

    // Extract finish_reason from the last choice that has one.
    let finish_reason = chunk
        .choices
        .iter()
        .rev()
        .find_map(|c| c.finish_reason.clone());

    // If the chunk includes usage info (requested via stream_options.include_usage),
    // extract it for the caller to include in the final Finished event.
    if let Some(usage) = chunk.usage {
        let prompt = usage.prompt_tokens.unwrap_or(0);
        let completion = usage.completion_tokens.unwrap_or(0);
        let cache_read = usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = usage
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);
        return (
            Vec::new(),
            Some(UsageInfo {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                reasoning_tokens: reasoning,
            }),
            finish_reason,
        );
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
                        //
                        // MiniMax sends name+arguments in the first delta, but
                        // arguments is just the JSON opening brace (e.g. "{").
                        // We must ALSO emit a ToolCallChunk for those partial
                        // arguments so the loop_llm.rs buffer accumulates the
                        // rest.  Only complete JSON (GLM/DeepSeek) should skip
                        // the ToolCallChunk.
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
                                arguments: String::new(), // arguments go through ToolCallChunk
                            },
                        }));
                        // Emit ToolCallChunk for any arguments present in the
                        // same delta.  This covers MiniMax partial-start
                        // (initial_args="{") and GLM/DeepSeek same-chunk
                        // (initial_args=complete JSON) alike.  The downstream
                        // loop_llm.rs will validate and deduplicate.
                        if !initial_arguments.is_empty() {
                            let idx = tc_delta.index.unwrap_or(0);
                            events.push(StreamEvent::ToolCallChunk {
                                index: idx,
                                arguments: initial_arguments,
                            });
                        }
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
                            events.push(StreamEvent::ToolCallChunk {
                                index: idx,
                                arguments: args,
                            });
                        }
                    }
                }
            }
        }
    }

    (events, None, finish_reason)
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
                content_parts: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: MessageRole::User,
                content: "Hello".to_string(),
                content_parts: None,
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
            content_parts: None,
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

        let custom =
            OpenAIProvider::with_base_url(Some("https://api.deepseek.com/v1"), Some("sk-test"));
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
        let resp = parse_response(
            msg_with_tc,
            Some(NativeUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        );
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

        println!(
            "Serialized NativeToolSpec: {}",
            serde_json::to_string_pretty(&serialized).unwrap()
        );
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
