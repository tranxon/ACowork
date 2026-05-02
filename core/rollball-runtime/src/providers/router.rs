//! LLM Provider router and factory
//!
//! Creates the appropriate Provider based on manifest LLM configuration.
//! Supports OpenAI-compatible, Anthropic, and Ollama providers.

use std::sync::Arc;

use rollball_core::providers::traits::Provider;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAIProvider;
use crate::providers::ollama::OllamaProvider;

/// Default base URL for MiniMax OpenAI-compatible API
const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimax.chat/v1";

/// Default base URL for ZhipuAI (智谱) OpenAI-compatible API
const ZHIPUAI_DEFAULT_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";

/// Default base URL for Moonshot AI (月之暗面) OpenAI-compatible API
const MOONSHOT_DEFAULT_BASE_URL: &str = "https://api.moonshot.cn/v1";

/// Default base URL for Alibaba/DashScope (通义千问) OpenAI-compatible API
const ALIBABA_DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";

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
        // ZhipuAI (智谱) — new ID: zhipuai, old IDs: zhipu, glm
        "zhipuai" | "zhipu" | "glm" => {
            tracing::info!(provider = provider_name, "Using ZhipuAI OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::with_base_url(
                    Some(ZHIPUAI_DEFAULT_BASE_URL),
                    api_key,
                )
            };
            Arc::new(provider)
        }
        // Moonshot AI (月之暗面) — new ID: moonshotai, old IDs: moonshot, kimi
        "moonshotai" | "moonshot" | "kimi" => {
            tracing::info!(provider = provider_name, "Using Moonshot OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::with_base_url(
                    Some(MOONSHOT_DEFAULT_BASE_URL),
                    api_key,
                )
            };
            Arc::new(provider)
        }
        // Alibaba/DashScope (通义千问) — new ID: alibaba, old IDs: qwen, dashscope
        "alibaba" | "qwen" | "dashscope" => {
            tracing::info!(provider = provider_name, "Using Alibaba/DashScope OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else {
                OpenAIProvider::with_base_url(
                    Some(ALIBABA_DEFAULT_BASE_URL),
                    api_key,
                )
            };
            Arc::new(provider)
        }
        // DeepSeek, Groq, Together AI, MiniMax, etc. are all OpenAI-compatible
        name if name.contains("deepseek")
            || name.contains("groq")
            || name.contains("together")
            || name.contains("fireworks")
            || name.contains("mistral")
            || name.contains("minimax") =>
        {
            tracing::info!(provider = name, "Using OpenAI-compatible provider");
            let provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url(Some(url), api_key)
            } else if name.contains("minimax") {
                // MiniMax China API — default base URL for OpenAI-compatible endpoint
                OpenAIProvider::with_base_url(
                    Some(MINIMAX_DEFAULT_BASE_URL),
                    api_key,
                )
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

/// Return a sensible default model name for a given provider.
/// Used when Gateway doesn't specify a model in LLMConfigDelivery.
pub fn default_model_for_provider(provider: &str) -> String {
    match provider {
        "openai" => "gpt-4o".to_string(),
        "anthropic" | "claude" => "claude-sonnet-4-20250514".to_string(),
        "google" | "gemini" => "gemini-2.5-flash".to_string(),
        "deepseek" => "deepseek-chat".to_string(),
        "zhipuai" | "zhipu" | "glm" => "glm-4-flash".to_string(),
        "moonshotai" | "moonshot" | "kimi" => "moonshot-v1-128k".to_string(),
        "alibaba" | "qwen" | "dashscope" => "qwen-max".to_string(),
        "groq" => "llama-4-scout-17b-16e-instruct".to_string(),
        "minimax" => "MiniMax-M2.5".to_string(),
        "mistral" => "mistral-large-latest".to_string(),
        "ollama" => "llama3".to_string(),
        _ => "gpt-4o".to_string(), // fallback
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
