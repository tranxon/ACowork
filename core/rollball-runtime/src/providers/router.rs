//! LLM Provider router and factory
//!
//! Creates the appropriate Provider based on provider ID.
//! Supports OpenAI-compatible, Anthropic, and Ollama protocols.
//!
//! DESIGN: Runtime always runs under Gateway. base_url and api_key are
//! delivered via LLMConfigDelivery from Gateway (which has full models.dev
//! offline data). Runtime's only job is selecting the correct PROTOCOL
//! (OpenAI vs Anthropic vs Ollama).
//!
//! If Gateway does not deliver a usable provider/model, Runtime refuses
//! service with a clear error — no silent fallbacks.

use std::sync::Arc;

use rollball_core::providers::traits::Provider;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAIProvider;
use crate::providers::ollama::OllamaProvider;

/// Create a provider based on the provider name.
///
/// Runtime's role is PROTOCOL SELECTION only:
///   - Anthropic providers -> AnthropicProvider
///   - Ollama -> OllamaProvider
///   - Everything else -> OpenAI-compatible (OpenAIProvider)
///
/// base_url is always supplied by Gateway LLMConfigDelivery.
/// If missing, the provider will likely fail - this is expected since
/// Runtime cannot function without Gateway.
pub fn create_provider(
    provider_name: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Arc<dyn Provider> {
    match provider_name {
        // ── Anthropic protocol providers ─────────────────────────────────
        // Per models.dev npm field: @ai-sdk/anthropic
        "anthropic" | "claude" | "minimax" | "minimax-cn" => {
            tracing::info!(provider = provider_name, "Using Anthropic protocol provider");
            let provider = if let Some(url) = base_url {
                AnthropicProvider::with_base_url(Some(url), api_key)
            } else {
                AnthropicProvider::new(api_key)
            };
            Arc::new(provider)
        }

        // ── Ollama protocol ──────────────────────────────────────────────
        "ollama" => {
            let provider = if let Some(url) = base_url {
                OllamaProvider::with_base_url(Some(url))
            } else {
                OllamaProvider::new()
            };
            Arc::new(provider)
        }

        // ── OpenAI-compatible providers ──────────────────────────────────
        // All other providers use OpenAI-compatible protocol.
        // Gateway delivers the correct base_url per provider.
        _ => {
            tracing::info!(provider = provider_name, "Using OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::new(api_key)
            };
            Arc::new(provider)
        }
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
        let provider = create_provider("openai", Some("sk-test"), None);
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_create_anthropic_provider() {
        let provider = create_provider("anthropic", Some("sk-ant-test"), None);
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_create_claude_provider() {
        let provider = create_provider("claude", Some("sk-ant-test"), None);
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_create_ollama_provider() {
        let provider = create_provider("ollama", None, None);
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn test_create_deepseek_provider() {
        let provider = create_provider("deepseek", Some("sk-test"), None);
        assert_eq!(provider.name(), "openai"); // Falls through to OpenAI-compatible
    }
}
