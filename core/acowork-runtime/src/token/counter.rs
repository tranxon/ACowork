//! Unified Token Counter
//!
//! Uses a single model→ratio lookup table ([`ModelRatioStore`]) for
//! token estimation (`tokens ≈ chars / ratio`). The ratio is calibrated
//! from LLM API feedback after each request:
//!
//! ```text
//! ratio = total_input_chars / prompt_tokens
//! ```
//!
//! Also provides:
//! - Incremental cache for system prompt token counting
//! - Full-field ChatMessage counting (role, name, tool_calls)
//! - Image token estimation per protocol type

use std::collections::HashMap;

use acowork_core::protocol::ProtocolType;
use acowork_core::providers::traits::{ChatMessage, MessageRole};

use super::ratio_store::ModelRatioStore;

// ── Image Token Estimation ──────────────────────────────────────────────

/// Estimate token count for an image based on protocol type.
///
/// Different LLM providers use different image tokenization strategies.
/// When width/height are unknown (None), a conservative default of 512×512 is used.
pub fn estimate_image_tokens(
    protocol_type: &ProtocolType,
    width: Option<u32>,
    height: Option<u32>,
    detail: Option<&str>,
) -> u64 {
    // Default to 512×512 when dimensions are unknown (conservative estimate).
    let w = width.unwrap_or(512) as u64;
    let h = height.unwrap_or(512) as u64;

    match protocol_type {
        ProtocolType::OpenAI => {
            // OpenAI: "low" detail uses fixed 85 tokens.
            // "high"/"auto" tiles the image at 512×512.
            if detail == Some("low") {
                return 85;
            }
            let tiles_w = w.div_ceil(512);
            let tiles_h = h.div_ceil(512);
            85 + 170 * tiles_w * tiles_h
        }
        ProtocolType::Anthropic => {
            // Anthropic: approximately 1 token per 750 pixels.
            (w * h) / 750
        }
        ProtocolType::Google => {
            // Google Gemini: approximately 1 token per 258 pixels.
            (w * h) / 258
        }
        ProtocolType::Ollama => {
            // Ollama models typically don't support vision.
            // Use conservative estimate for any vision-capable Ollama models.
            (w * h) / 258
        }
    }
}

// ── Token Counter ───────────────────────────────────────────────────────

/// Unified token counter backed by a [`ModelRatioStore`].
///
/// All token estimation uses `chars / ratio` where the ratio is lazily
/// calibrated from LLM API feedback. No static sampling ratios or
/// hard-coded tier classification.
pub struct TokenCounter {
    /// Cached system prompt tokens (avoids recounting on every turn)
    system_prompt_cache: HashMap<String, u64>,
    /// Model → chars/token ratio lookup
    model_ratios: ModelRatioStore,
}

impl TokenCounter {
    /// Create a new token counter with an empty, in-memory ratio store.
    /// Uses default ratio 3.5 for all models until calibrated.
    pub fn new() -> Self {
        Self {
            system_prompt_cache: HashMap::new(),
            model_ratios: ModelRatioStore::new(),
        }
    }

    /// Create a new token counter with a pre-configured ratio store.
    pub fn new_with_ratios(model_ratios: ModelRatioStore) -> Self {
        Self {
            system_prompt_cache: HashMap::new(),
            model_ratios,
        }
    }

    /// Access the model ratio store for mutable operations (e.g. calibration).
    pub fn model_ratios_mut(&mut self) -> &mut ModelRatioStore {
        &mut self.model_ratios
    }

    /// Access the model ratio store (read-only).
    pub fn model_ratios(&self) -> &ModelRatioStore {
        &self.model_ratios
    }

    // ── Text counting ───────────────────────────────────────────────────

    /// Count tokens for a single text string using the unified ratio table.
    ///
    /// `tokens = ceil(text.len() / ratio)`
    pub fn count_text(&self, text: &str, model: &str) -> u64 {
        self.count_text_ratio(text, model)
    }

    /// Internal: ratio-based text token counting.
    fn count_text_ratio(&self, text: &str, model: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let ratio = self.model_ratios.get(model);
        (text.len() as f64 / ratio).ceil() as u64
    }

    // ── Message counting ────────────────────────────────────────────────

    /// Count tokens for a full ChatMessage (including role, name, tool_calls overhead).
    /// When `protocol_type` is provided, image content parts are included in the count.
    pub fn count_message(
        &self,
        message: &ChatMessage,
        model: &str,
        protocol_type: Option<&ProtocolType>,
    ) -> u64 {
        let mut tokens = 0u64;

        // Role overhead: ~1 token for role marker
        tokens += 1;

        // Name overhead: ~1 token per 4 chars + 1 for the name field
        if let Some(ref name) = message.name {
            tokens += self.count_text(name, model) + 1;
        }

        // Content tokens: prefer content_parts if available, else fall back to .content
        if let Some(ref parts) = message.content_parts {
            for part in parts {
                match part {
                    acowork_core::providers::traits::ContentPart::Text { text } => {
                        tokens += self.count_text(text, model);
                    }
                    acowork_core::providers::traits::ContentPart::ImageUrl { image_url } => {
                        if let Some(pt) = protocol_type {
                            tokens += estimate_image_tokens(
                                pt,
                                image_url.width,
                                image_url.height,
                                image_url.detail.as_deref(),
                            );
                        }
                        // If protocol_type is unknown, skip image tokens (best-effort)
                    }
                }
            }
        } else {
            tokens += self.count_text(&message.content, model);
        }

        // Tool calls overhead
        if let Some(ref tool_calls) = message.tool_calls {
            for tc in tool_calls {
                // Each tool call has overhead: id + type + function wrapper ~4 tokens
                tokens += 4;
                // Function name
                tokens += self.count_text(&tc.function.name, model);
                // Function arguments
                tokens += self.count_text(&tc.function.arguments, model);
            }
        }

        // Message boundary token (varies by API but typically 1)
        tokens += 1;

        tokens
    }

    /// Count tokens for a list of messages with system prompt caching
    pub fn count_messages(&mut self, messages: &[ChatMessage], model: &str) -> u64 {
        let mut total = 0u64;

        for msg in messages {
            if matches!(msg.role, MessageRole::System) {
                // Use cached count for system prompt if available
                let cache_key = format!("{}:{}", model, msg.content.len());
                if let Some(&cached) = self.system_prompt_cache.get(&cache_key) {
                    total += cached;
                } else {
                    let count = self.count_message(msg, model, None);
                    self.system_prompt_cache.insert(cache_key, count);
                    total += count;
                }
            } else {
                total += self.count_message(msg, model, None);
            }
        }

        total
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::{FunctionCall, ToolCall};

    #[test]
    fn test_count_text_default_ratio() {
        let counter = TokenCounter::new();
        let text = "Hello, how are you today?";
        let chars = text.len() as f64;
        let ratio = 3.5;
        let expected = (chars / ratio).ceil() as u64;
        let count = counter.count_text(text, "some-model");
        assert_eq!(count, expected);
    }

    #[test]
    fn test_count_text_calibrated_ratio() {
        let mut store = ModelRatioStore::new();
        store.update("custom-model", 4.0);
        let counter = TokenCounter::new_with_ratios(store);
        let text = "the quick brown fox jumps over the lazy dog";
        // 43 chars / 4.0 = 10.75 → ceil → 11
        let count = counter.count_text(text, "custom-model");
        assert_eq!(count, 11);
    }

    #[test]
    fn test_count_text_cjk() {
        let counter = TokenCounter::new();
        let text = "你好世界，今天天气不错";
        let count = counter.count_text(text, "gpt-4");
        assert!(
            count >= 3,
            "Expected at least 3 tokens for CJK text, got {count}"
        );
    }

    #[test]
    fn test_count_text_mixed() {
        let counter = TokenCounter::new();
        let text = "Hello 你好 world 世界";
        let count = counter.count_text(text, "gpt-4");
        assert!(count >= 3, "Expected at least 3 tokens, got {count}");
    }

    #[test]
    fn test_count_message_basic() {
        let counter = TokenCounter::new();
        let msg = ChatMessage::user("Hello world");
        let count = counter.count_message(&msg, "gpt-4", None);
        // content tokens + role overhead + boundary
        assert!(count >= 3, "Expected at least 3 tokens, got {count}");
    }

    #[test]
    fn test_count_message_with_name() {
        let counter = TokenCounter::new();
        let msg = ChatMessage {
            role: MessageRole::User,
            content: "Hello".to_string(),
            name: Some("Alice".to_string()),
            ..Default::default()
        };
        let count_without_name = counter.count_text("Hello", "gpt-4") + 2; // role + boundary
        let count_with_name = counter.count_message(&msg, "gpt-4", None);
        assert!(
            count_with_name > count_without_name,
            "Named message should have more tokens"
        );
    }

    #[test]
    fn test_count_message_with_tool_calls() {
        let counter = TokenCounter::new();
        let msg = ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "weather".to_string(),
                    arguments: r#"{"city":"Shanghai"}"#.to_string(),
                },
            }],
        );
        let count = counter.count_message(&msg, "gpt-4", None);
        // Tool call overhead (4) + name + arguments + role + boundary
        assert!(
            count >= 6,
            "Expected at least 6 tokens for tool call message, got {count}"
        );
    }

    #[test]
    fn test_count_messages_with_cache() {
        let mut counter = TokenCounter::new();
        let system = ChatMessage::system("You are a helpful assistant. Be concise and accurate.");
        let user = ChatMessage::user("Hello");

        let count1 = counter.count_messages(&[system.clone(), user.clone()], "gpt-4");
        // Second call should use cache for system prompt
        let count2 = counter.count_messages(&[system, user], "gpt-4");
        assert_eq!(count1, count2, "Cached count should be consistent");
        assert!(
            !counter.system_prompt_cache.is_empty(),
            "Cache should be populated"
        );
    }

    #[test]
    fn test_count_text_empty() {
        let counter = TokenCounter::new();
        assert_eq!(counter.count_text("", "gpt-4"), 0);
    }

    #[test]
    fn test_count_text_single_char() {
        let counter = TokenCounter::new();
        let count = counter.count_text("a", "gpt-4");
        assert!(count >= 1);
    }
}
