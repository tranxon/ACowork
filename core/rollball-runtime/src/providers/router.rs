//! LLM Provider router and factory
//!
//! Creates the appropriate Provider based on manifest LLM configuration.
//! Supports OpenAI-compatible, Anthropic, and Ollama providers.

use std::sync::Arc;

use rollball_core::providers::traits::Provider;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAIProvider;
use crate::providers::ollama::OllamaProvider;

/// Create a provider based on the provider name from manifest
pub fn create_provider(
    provider_name: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Arc<dyn Provider> {
    match provider_name {
        "anthropic" | "claude" => {
            let provider = if let Some(url) = base_url {
                AnthropicProvider::with_base_url(Some(url), api_key)
            } else {
                AnthropicProvider::new(api_key)
            };
            Arc::new(provider)
        }
        "openai" | "openai-compatible" => {
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::new(api_key)
            };
            Arc::new(provider)
        }
        "ollama" => {
            let provider = if let Some(url) = base_url {
                OllamaProvider::with_base_url(Some(url))
            } else {
                OllamaProvider::new()
            };
            Arc::new(provider)
        }
        // DeepSeek, Groq, Together AI, etc. are all OpenAI-compatible
        name if name.contains("deepseek")
            || name.contains("groq")
            || name.contains("together")
            || name.contains("fireworks")
            || name.contains("mistral") =>
        {
            tracing::info!(provider = name, "Using OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::new(api_key)
            };
            Arc::new(provider)
        }
        _ => {
            tracing::warn!(
                provider = provider_name,
                "Unknown provider, falling back to OpenAI-compatible"
            );
            Arc::new(OpenAIProvider::new(api_key))
        }
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
