//! LLM adapter — bridges `acowork_core::providers::traits::Provider` to
//! `acowork_grafeo::consolidation::triple_extraction::TripleExtractorLlm`.
//!
//! The grafeo crate defines a minimal LLM trait (`TripleExtractorLlm`) so it
//! stays independent of the runtime's provider ecosystem. This adapter wraps
//! a `dyn Provider` (which has a full chat API with streaming, tool calls, etc.)
//! into the simple `async chat(messages) -> response` interface that grafeo
//! expects, using a low-temperature non-streaming call.

use acowork_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider};
use acowork_grafeo::consolidation::triple_extraction::{
    LlmMessage, LlmResponse, TripleExtractorLlm,
};

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Adapter: wraps a `dyn Provider` as a `TripleExtractorLlm`.
///
/// Uses a fixed low temperature (0.1) and no tool-calling to get
/// deterministic structured output from the LLM.
pub struct ProviderLlmAdapter {
    provider: std::sync::Arc<dyn Provider>,
    model: String,
}

impl ProviderLlmAdapter {
    /// Create a new adapter from a Provider and model name.
    pub fn new(provider: std::sync::Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }
}

#[async_trait::async_trait]
impl TripleExtractorLlm for ProviderLlmAdapter {
    async fn chat(&self, messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String> {
        // Convert grafeo LlmMessage → acowork-core ChatMessage.
        let chat_messages: Vec<ChatMessage> = messages
            .iter()
            .map(|m| {
                let role = match m.role.as_str() {
                    "system" => MessageRole::System,
                    "assistant" => MessageRole::Assistant,
                    _ => MessageRole::User,
                };
                ChatMessage {
                    role,
                    content: m.content.clone(),
                    ..Default::default()
                }
            })
            .collect();

        let request = ChatRequest {
            model: self.model.clone(),
            messages: chat_messages,
            temperature: Some(0.1), // Low temperature for structured extraction
            max_tokens: Some(2048), // Enough for triple arrays / classification JSON
            tools: None,            // No tool calling for extraction tasks
            reasoning_effort: None,
            thinking_mode: None,
        };

        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| format!("Provider chat failed: {}", e))?;

        Ok(LlmResponse {
            content: response.content,
            usage_tokens: response.usage.map(|u| u.total_tokens),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::mock::MockProvider;

    #[tokio::test]
    async fn test_adapter_converts_messages_and_returns_response() {
        let provider = std::sync::Arc::new(MockProvider::single_text(
            r#"[{"subject":"user","predicate":"likes","object":"Rust","confidence":0.9,"sub_type":"fact"}]"#,
        ));
        let adapter = ProviderLlmAdapter::new(provider, "mock-model".to_string());

        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: "You are a knowledge extractor.".to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: "I love Rust programming".to_string(),
            },
        ];

        let response = adapter.chat(messages).await.unwrap();
        assert!(response.content.contains("user"));
        assert!(response.content.contains("Rust"));
    }

    #[tokio::test]
    async fn test_adapter_handles_provider_error() {
        // MockProvider that always returns empty content (simulating graceful degradation)
        let provider = std::sync::Arc::new(MockProvider::single_text(""));
        let adapter = ProviderLlmAdapter::new(provider, "mock-model".to_string());

        let messages = vec![LlmMessage {
            role: "user".to_string(),
            content: "test".to_string(),
        }];

        let response = adapter.chat(messages).await.unwrap();
        assert!(response.content.is_empty());
    }
}
