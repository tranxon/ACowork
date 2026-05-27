//! LLM Provider router and factory
//!
//! Creates the appropriate Provider based on protocol type.
//! Supports OpenAI-compatible, Anthropic, Google Gemini, and Ollama protocols.
//!
//! DESIGN: Runtime always runs under Gateway. base_url and api_key are
//! delivered via LLMConfigDelivery from Gateway (which has full models.dev
//! offline data). Protocol selection is data-driven via ProtocolType —
//! no hardcoded provider name matching.
//!
//! If Gateway does not deliver a usable provider/model, Runtime refuses
//! service with a clear error — no silent fallbacks.

use std::sync::Arc;

use rollball_core::ProtocolType;
use std::time::Duration;

use rollball_core::providers::traits::Provider;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAIProvider;
use crate::providers::ollama::OllamaProvider;

/// Bundle of timeout values passed from RuntimeConfig to provider constructors.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTimeouts {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub stream_read_timeout: Duration,
}

impl From<&crate::config::RuntimeConfig> for ProviderTimeouts {
    fn from(config: &crate::config::RuntimeConfig) -> Self {
        Self {
            request_timeout: Duration::from_millis(config.provider_request_timeout_ms),
            connect_timeout: Duration::from_millis(config.provider_connect_timeout_ms),
            stream_read_timeout: Duration::from_millis(config.provider_stream_read_timeout_ms),
        }
    }
}

impl ProviderTimeouts {
    /// Default timeouts (10min request, 10s connect, 45s stream read).
    pub fn defaults() -> Self {
        Self {
            request_timeout: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(10),
            stream_read_timeout: Duration::from_secs(45),
        }
    }
}

/// Create a provider based on protocol type.
///
/// Protocol selection is data-driven: Gateway determines the correct
/// ProtocolType from offline provider metadata and delivers it via
/// LLMConfigDelivery. Runtime simply instantiates the matching provider.
///
/// base_url is always supplied by Gateway LLMConfigDelivery.
/// If missing, the provider will likely fail - this is expected since
/// Runtime cannot function without Gateway.
pub fn create_provider(
    provider_name: &str,
    protocol_type: &ProtocolType,
    api_key: Option<&str>,
    base_url: Option<&str>,
    timeouts: Option<ProviderTimeouts>,
) -> Arc<dyn Provider> {
    let t = timeouts.unwrap_or_else(ProviderTimeouts::defaults);
    match protocol_type {
        ProtocolType::Anthropic => {
            tracing::info!(provider = provider_name, "Using Anthropic protocol provider");
            let provider = if let Some(url) = base_url {
                AnthropicProvider::with_base_url_and_timeouts(Some(url), api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            } else {
                AnthropicProvider::with_base_url_and_timeouts(None, api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            };
            Arc::new(provider)
        }

        ProtocolType::Google => {
            // Google Gemini uses OpenAI-compatible protocol via OpenAIProvider
            // until a native GoogleProvider is implemented.
            tracing::info!(provider = provider_name, "Using Google Gemini (OpenAI-compatible) provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url_and_timeouts(Some(url), api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            } else {
                OpenAIProvider::with_base_url_and_timeouts(None, api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            };
            Arc::new(provider)
        }

        ProtocolType::Ollama => {
            let provider = if let Some(url) = base_url {
                OllamaProvider::with_base_url_and_timeouts(Some(url), t.request_timeout, t.connect_timeout)
            } else {
                OllamaProvider::with_base_url_and_timeouts(None, t.request_timeout, t.connect_timeout)
            };
            Arc::new(provider)
        }

        ProtocolType::OpenAI => {
            tracing::info!(provider = provider_name, "Using OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url_and_timeouts(Some(url), api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            } else {
                OpenAIProvider::with_base_url_and_timeouts(None, api_key, t.request_timeout, t.connect_timeout, t.stream_read_timeout)
            };
            Arc::new(provider)
        }
    }
}

/// Infer protocol type from provider name for standalone mode.
///
/// In standalone mode (no Gateway), there is no offline metadata to
/// determine protocol type. This function provides a fallback inference
/// based on the provider name. Gateway mode should always use the
/// protocol_type from LLMConfigDelivery instead.
pub fn infer_protocol_type(provider_name: &str) -> ProtocolType {
    match provider_name {
        "anthropic" | "claude" | "minimax" | "minimax-cn" => ProtocolType::Anthropic,
        "ollama" => ProtocolType::Ollama,
        "google" | "gemini" => ProtocolType::Google,
        _ => ProtocolType::OpenAI,
    }
}

/// Create a no-op provider that always returns an error.
/// Used when no LLM config is available (Gateway mode without API key).
pub fn create_noop_provider() -> Arc<dyn Provider> {
    Arc::new(NoopProvider)
}

/// A provider that always returns an error, used when no LLM config is available.
struct NoopProvider;

#[async_trait::async_trait]
impl Provider for NoopProvider {
    fn name(&self) -> &str { "noop" }

    async fn chat(
        &self,
        _request: rollball_core::providers::traits::ChatRequest,
    ) -> rollball_core::error::Result<rollball_core::providers::traits::ChatResponse> {
        Err(rollball_core::error::RollballError::Provider(
            rollball_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured. Please add an API key in Desktop App Settings.".to_string(),
            )
        ))
    }

    async fn chat_stream(
        &self,
        _request: rollball_core::providers::traits::ChatRequest,
    ) -> rollball_core::error::Result<Box<dyn futures_core::Stream<Item = rollball_core::providers::traits::StreamEvent> + Send>> {
        Err(rollball_core::error::RollballError::Provider(
            rollball_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured. Please add an API key in Desktop App Settings.".to_string(),
            )
        ))
    }

    async fn chat_token_count(
        &self,
        _messages: &[rollball_core::providers::traits::ChatMessage],
    ) -> rollball_core::error::Result<u64> {
        Err(rollball_core::error::RollballError::Provider(
            rollball_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured.".to_string(),
            )
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_openai_provider() {
        let provider = create_provider("openai", &ProtocolType::OpenAI, Some("sk-test"), None, None);
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_create_anthropic_provider() {
        let provider = create_provider("anthropic", &ProtocolType::Anthropic, Some("sk-ant-test"), None, None);
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_create_ollama_provider() {
        let provider = create_provider("ollama", &ProtocolType::Ollama, None, None, None);
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn test_create_deepseek_provider() {
        let provider = create_provider("deepseek", &ProtocolType::OpenAI, Some("sk-test"), None, None);
        assert_eq!(provider.name(), "openai"); // Uses OpenAI-compatible protocol
    }

    #[test]
    fn test_infer_protocol_type() {
        assert_eq!(infer_protocol_type("anthropic"), ProtocolType::Anthropic);
        assert_eq!(infer_protocol_type("claude"), ProtocolType::Anthropic);
        assert_eq!(infer_protocol_type("minimax"), ProtocolType::Anthropic);
        assert_eq!(infer_protocol_type("ollama"), ProtocolType::Ollama);
        assert_eq!(infer_protocol_type("openai"), ProtocolType::OpenAI);
        assert_eq!(infer_protocol_type("deepseek"), ProtocolType::OpenAI);
        assert_eq!(infer_protocol_type("unknown-provider"), ProtocolType::OpenAI);
    }
}
