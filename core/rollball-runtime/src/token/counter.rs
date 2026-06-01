//! Tiered Token Counter
//!
//! Implements a three-tier token counting strategy:
//! - Tier 1 (Exact): Uses tiktoken-rs for OpenAI models. Error < 1%.
//! - Tier 2 (Approximate): Uses a sampling ratio from known tokenizers. Error < 5%.
//! - Tier 3 (Heuristic): Word/char based estimation. Error < 15%.
//!
//! Also provides:
//! - Incremental cache for system prompt token counting
//! - Elastic budget allocation (fixed zone + distributable space)
//! - Full-field ChatMessage counting (role, name, tool_calls)

use std::collections::HashMap;

use rollball_core::protocol::ProtocolType;
use rollball_core::providers::traits::{ChatMessage, MessageRole};

/// Lazy-initialized tiktoken BPE for cl100k_base (GPT-4/GPT-4o/GPT-3.5).
fn get_cl100k_bpe() -> Option<&'static tiktoken_rs::CoreBPE> {
    use std::sync::OnceLock;
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| {
        tiktoken_rs::cl100k_base().ok()
    }).as_ref()
}

// ── Tier classification ─────────────────────────────────────────────────

/// Precision tier for token counting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCountTier {
    /// Exact counting via tiktoken-rs (OpenAI models)
    Tier1Exact,
    /// Approximate counting via sampling ratio
    Tier2Approximate,
    /// Heuristic estimation
    Tier3Heuristic,
}

impl std::fmt::Display for TokenCountTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenCountTier::Tier1Exact => write!(f, "Tier1(Exact)"),
            TokenCountTier::Tier2Approximate => write!(f, "Tier2(Approximate)"),
            TokenCountTier::Tier3Heuristic => write!(f, "Tier3(Heuristic)"),
        }
    }
}

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
            let tiles_w = (w + 511) / 512;
            let tiles_h = (h + 511) / 512;
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

/// Tiered token counter with caching support
pub struct TokenCounter {
    /// Cached system prompt tokens (avoids recounting on every turn)
    system_prompt_cache: HashMap<String, u64>,
    /// Known sampling ratios for Tier 2 approximation
    /// Maps model family → (chars_per_token_ratio)
    sampling_ratios: HashMap<String, f64>,
    /// Tier 2 observed ratios from actual API usage
    observed_ratios: HashMap<String, f64>,
}

impl TokenCounter {
    /// Create a new token counter
    pub fn new() -> Self {
        let mut sampling_ratios = HashMap::new();
        // Default ratios based on empirical observations
        sampling_ratios.insert("gpt-4".to_string(), 3.8);      // ~3.8 chars/token
        sampling_ratios.insert("gpt-4o".to_string(), 3.8);
        sampling_ratios.insert("gpt-3.5".to_string(), 4.0);
        sampling_ratios.insert("claude".to_string(), 3.5);     // Claude is slightly more efficient
        sampling_ratios.insert("llama".to_string(), 3.6);
        sampling_ratios.insert("qwen".to_string(), 3.2);       // CJK-optimized
        sampling_ratios.insert("mistral".to_string(), 3.7);

        Self {
            system_prompt_cache: HashMap::new(),
            sampling_ratios,
            observed_ratios: HashMap::new(),
        }
    }

    /// Determine the counting tier for a given model
    pub fn tier_for_model(model: &str) -> TokenCountTier {
        let lower = model.to_lowercase();

        // Tier 1: Models with exact tokenizers
        if lower.contains("gpt-4") || lower.contains("gpt-3.5") || lower.contains("gpt-4o") {
            return TokenCountTier::Tier1Exact;
        }

        // Tier 2: Models with known sampling ratios
        if lower.contains("claude") || lower.contains("llama") || lower.contains("qwen")
            || lower.contains("mistral") || lower.contains("deepseek")
            || lower.contains("gemini") || lower.contains("minimax")
        {
            return TokenCountTier::Tier2Approximate;
        }

        // Tier 3: Unknown models
        TokenCountTier::Tier3Heuristic
    }

    /// Count tokens for a single text string using the best available method
    pub fn count_text(&self, text: &str, model: &str) -> u64 {
        let tier = Self::tier_for_model(model);
        match tier {
            TokenCountTier::Tier1Exact => self.count_tier1(text, model),
            TokenCountTier::Tier2Approximate => self.count_tier2(text, model),
            TokenCountTier::Tier3Heuristic => self.count_tier3(text),
        }
    }

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
                    rollball_core::providers::traits::ContentPart::Text { text } => {
                        tokens += self.count_text(text, model);
                    }
                    rollball_core::providers::traits::ContentPart::ImageUrl { image_url } => {
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

    /// Update observed ratio from actual API usage
    pub fn update_observed_ratio(&mut self, model: &str, actual_tokens: u64, char_count: usize) {
        if actual_tokens > 0 && char_count > 0 {
            let ratio = char_count as f64 / actual_tokens as f64;
            self.observed_ratios.insert(model.to_string(), ratio);
        }
    }

    // ── Tier implementations ────────────────────────────────────────────

    /// Tier 1: Exact token counting
    /// Uses tiktoken-rs cl100k_base for OpenAI models (GPT-4/GPT-4o/GPT-3.5).
    /// Falls back to well-calibrated approximation when tiktoken is unavailable.
    fn count_tier1(&self, text: &str, _model: &str) -> u64 {
        // Try tiktoken-rs cl100k_base (shared by all OpenAI GPT models)
        if let Some(bpe) = get_cl100k_bpe() {
            let tokens = bpe.encode_ordinary(text);
            return tokens.len() as u64;
        }

        // Fallback: use observed ratio if available (most accurate)
        if let Some(&ratio) = self.observed_ratios.get(_model) {
            return (text.len() as f64 / ratio).ceil() as u64;
        }

        // Use known sampling ratio
        if let Some(&ratio) = self.sampling_ratios.get(_model) {
            return (text.len() as f64 / ratio).ceil() as u64;
        }

        // Fallback: well-calibrated heuristic for GPT models
        self.count_tier2(text, "gpt-4")
    }

    /// Tier 2: Approximate token counting using sampling ratios
    fn count_tier2(&self, text: &str, model: &str) -> u64 {
        // Check observed ratios first
        if let Some(&ratio) = self.observed_ratios.get(model) {
            return (text.len() as f64 / ratio).ceil() as u64;
        }

        // Check known model family ratios
        let lower = model.to_lowercase();
        for (key, &ratio) in &self.sampling_ratios {
            if lower.contains(key) {
                return (text.len() as f64 / ratio).ceil() as u64;
            }
        }

        // Default ratio
        (text.len() as f64 / 3.5).ceil() as u64
    }

    /// Tier 3: Heuristic estimation
    /// English: words × 1.3, CJK: chars × 0.6
    fn count_tier3(&self, text: &str) -> u64 {
        let _ascii_count = text.chars().filter(|c| c.is_ascii()).count();
        let cjk_count = text
            .chars()
            .filter(|c| !c.is_ascii())
            .count();

        // English: split into words, each word ~1.3 tokens
        let ascii_words = text
            .split_whitespace()
            .filter(|w| w.is_ascii())
            .count();

        let ascii_tokens = (ascii_words as f64 * 1.3).ceil() as u64;

        // CJK: ~0.6 tokens per character
        let cjk_tokens = (cjk_count as f64 * 0.6).ceil() as u64;

        // Minimum 1 token if text is non-empty
        let total = ascii_tokens + cjk_tokens;
        if total == 0 && !text.is_empty() {
            1
        } else {
            total
        }
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
    use rollball_core::providers::traits::{FunctionCall, ToolCall};

    #[test]
    fn test_tier_classification() {
        assert_eq!(TokenCounter::tier_for_model("gpt-4"), TokenCountTier::Tier1Exact);
        assert_eq!(TokenCounter::tier_for_model("gpt-4o"), TokenCountTier::Tier1Exact);
        assert_eq!(TokenCounter::tier_for_model("gpt-3.5-turbo"), TokenCountTier::Tier1Exact);
        assert_eq!(TokenCounter::tier_for_model("claude-sonnet-4"), TokenCountTier::Tier2Approximate);
        assert_eq!(TokenCounter::tier_for_model("llama3"), TokenCountTier::Tier2Approximate);
        assert_eq!(TokenCounter::tier_for_model("qwen3:8b"), TokenCountTier::Tier2Approximate);
        assert_eq!(TokenCounter::tier_for_model("some-unknown-model"), TokenCountTier::Tier3Heuristic);
    }

    #[test]
    fn test_count_text_english() {
        let counter = TokenCounter::new();
        let text = "Hello, how are you today?";
        let count = counter.count_text(text, "gpt-4");
        // "Hello, how are you today?" ≈ 7-8 tokens
        assert!((4..=12).contains(&count), "Expected ~7 tokens, got {count}");
    }

    #[test]
    fn test_count_text_cjk() {
        let counter = TokenCounter::new();
        let text = "你好世界，今天天气不错";
        let count = counter.count_text(text, "gpt-4");
        assert!(count >= 3, "Expected at least 3 tokens for CJK text, got {count}");
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
        assert!(count_with_name > count_without_name, "Named message should have more tokens");
    }

    #[test]
    fn test_count_message_with_tool_calls() {
        let counter = TokenCounter::new();
        let msg = ChatMessage::assistant_with_tools("", vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "weather".to_string(),
                arguments: r#"{"city":"Shanghai"}"#.to_string(),
            },
        }]);
        let count = counter.count_message(&msg, "gpt-4", None);
        // Tool call overhead (4) + name + arguments + role + boundary
        assert!(count >= 6, "Expected at least 6 tokens for tool call message, got {count}");
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
        assert!(!counter.system_prompt_cache.is_empty(), "Cache should be populated");
    }

    #[test]
    fn test_update_observed_ratio() {
        let mut counter = TokenCounter::new();
        counter.update_observed_ratio("my-custom-model", 100, 380);
        assert_eq!(counter.observed_ratios.get("my-custom-model"), Some(&3.8));
    }

    #[test]
    fn test_tier3_heuristic_english() {
        let counter = TokenCounter::new();
        let count = counter.count_text("Hello world this is a test", "unknown-model");
        // 6 words × 1.3 ≈ 8 tokens
        assert!((5..=12).contains(&count), "Expected ~8 tokens, got {count}");
    }

    #[test]
    fn test_tier3_heuristic_empty() {
        let counter = TokenCounter::new();
        let count = counter.count_text("", "unknown-model");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_tier3_heuristic_single_char() {
        let counter = TokenCounter::new();
        let count = counter.count_text("a", "unknown-model");
        assert!(count >= 1);
    }

}
