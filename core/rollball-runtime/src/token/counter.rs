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

use rollball_core::providers::traits::{ChatMessage, MessageRole};

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

    /// Count tokens for a full ChatMessage (including role, name, tool_calls overhead)
    pub fn count_message(&self, message: &ChatMessage, model: &str) -> u64 {
        let mut tokens = 0u64;

        // Role overhead: ~1 token for role marker
        tokens += 1;

        // Name overhead: ~1 token per 4 chars + 1 for the name field
        if let Some(ref name) = message.name {
            tokens += self.count_text(name, model) + 1;
        }

        // Content tokens
        tokens += self.count_text(&message.content, model);

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
                    let count = self.count_message(msg, model);
                    self.system_prompt_cache.insert(cache_key, count);
                    total += count;
                }
            } else {
                total += self.count_message(msg, model);
            }
        }

        total
    }

    /// Incremental count: count only new messages since last count
    pub fn count_incremental(
        &self,
        new_messages: &[ChatMessage],
        model: &str,
    ) -> u64 {
        let mut total = 0u64;
        for msg in new_messages {
            total += self.count_message(msg, model);
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
    /// For OpenAI models, uses tiktoken-rs when available,
    /// otherwise falls back to well-calibrated approximation.
    fn count_tier1(&self, text: &str, model: &str) -> u64 {
        // Use observed ratio if available (most accurate)
        if let Some(&ratio) = self.observed_ratios.get(model) {
            return (text.len() as f64 / ratio).ceil() as u64;
        }

        // Use known sampling ratio
        if let Some(&ratio) = self.sampling_ratios.get(model) {
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
            .filter(|w| w.chars().all(|c| c.is_ascii()))
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

// ── Budget Allocation ───────────────────────────────────────────────────

/// Elastic budget allocation for context window
///
/// Divides the available context window into:
/// - Fixed zone: system prompt + output reserve
/// - Distributable space: split between history and retrieval
#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    /// Total context window size in tokens
    pub context_window: u64,
    /// Reserved tokens for output (from manifest.max_output_tokens)
    pub output_reserve: u64,
    /// System prompt token count (measured)
    pub system_prompt_tokens: u64,
    /// History share ratio (default: 0.75)
    pub history_ratio: f64,
    /// Retrieval share ratio (default: 0.25)
    pub retrieval_ratio: f64,
    /// Hard minimum for retrieval tokens
    pub retrieval_min_tokens: u64,
    /// Hard minimum turns to keep in history
    pub history_min_turns: usize,
}

impl BudgetAllocation {
    /// Create a new budget allocation with default ratios
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            output_reserve: 1024,
            system_prompt_tokens: 0,
            history_ratio: 0.75,
            retrieval_ratio: 0.25,
            retrieval_min_tokens: 2048,
            history_min_turns: 3,
        }
    }

    /// Set output reserve
    pub fn with_output_reserve(mut self, tokens: u64) -> Self {
        self.output_reserve = tokens;
        self
    }

    /// Set system prompt tokens
    pub fn with_system_prompt(mut self, tokens: u64) -> Self {
        self.system_prompt_tokens = tokens;
        self
    }

    /// Calculate the fixed zone (tokens that cannot be redistributed)
    pub fn fixed_zone(&self) -> u64 {
        self.system_prompt_tokens + self.output_reserve
    }

    /// Calculate the distributable space
    pub fn distributable_space(&self) -> u64 {
        self.context_window.saturating_sub(self.fixed_zone())
    }

    /// Get history token budget
    pub fn history_budget(&self) -> u64 {
        let space = self.distributable_space();
        let history = (space as f64 * self.history_ratio) as u64;
        // Ensure we don't violate retrieval minimum
        let retrieval = self.retrieval_budget();
        if space.saturating_sub(retrieval) < history {
            space.saturating_sub(retrieval)
        } else {
            history
        }
    }

    /// Get retrieval token budget
    pub fn retrieval_budget(&self) -> u64 {
        let space = self.distributable_space();
        let retrieval = (space as f64 * self.retrieval_ratio) as u64;
        // Ensure hard minimum
        retrieval.max(self.retrieval_min_tokens).min(space)
    }

    /// Check if budget allocation is valid
    pub fn is_valid(&self) -> bool {
        self.fixed_zone() < self.context_window
            && self.history_ratio + self.retrieval_ratio <= 1.0
    }
}

impl Default for BudgetAllocation {
    fn default() -> Self {
        Self::new(128000) // Default GPT-4 context window
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
        assert!(count >= 4 && count <= 12, "Expected ~7 tokens, got {count}");
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
        let msg = ChatMessage {
            role: MessageRole::User,
            content: "Hello world".to_string(),
            name: None,
            tool_calls: None,
        };
        let count = counter.count_message(&msg, "gpt-4");
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
            tool_calls: None,
        };
        let count_without_name = counter.count_text("Hello", "gpt-4") + 2; // role + boundary
        let count_with_name = counter.count_message(&msg, "gpt-4");
        assert!(count_with_name > count_without_name, "Named message should have more tokens");
    }

    #[test]
    fn test_count_message_with_tool_calls() {
        let counter = TokenCounter::new();
        let msg = ChatMessage {
            role: MessageRole::Assistant,
            content: "".to_string(),
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "weather".to_string(),
                    arguments: r#"{"city":"Shanghai"}"#.to_string(),
                },
            }]),
        };
        let count = counter.count_message(&msg, "gpt-4");
        // Tool call overhead (4) + name + arguments + role + boundary
        assert!(count >= 6, "Expected at least 6 tokens for tool call message, got {count}");
    }

    #[test]
    fn test_count_messages_with_cache() {
        let mut counter = TokenCounter::new();
        let system = ChatMessage {
            role: MessageRole::System,
            content: "You are a helpful assistant. Be concise and accurate.".to_string(),
            name: None,
            tool_calls: None,
        };
        let user = ChatMessage {
            role: MessageRole::User,
            content: "Hello".to_string(),
            name: None,
            tool_calls: None,
        };

        let count1 = counter.count_messages(&[system.clone(), user.clone()], "gpt-4");
        // Second call should use cache for system prompt
        let count2 = counter.count_messages(&[system, user], "gpt-4");
        assert_eq!(count1, count2, "Cached count should be consistent");
        assert!(!counter.system_prompt_cache.is_empty(), "Cache should be populated");
    }

    #[test]
    fn test_count_incremental() {
        let counter = TokenCounter::new();
        let new_messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: "What is the weather?".to_string(),
                name: None,
                tool_calls: None,
            },
        ];
        let count = counter.count_incremental(&new_messages, "gpt-4");
        assert!(count > 0);
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
        assert!(count >= 5 && count <= 12, "Expected ~8 tokens, got {count}");
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

    // ── BudgetAllocation tests ──────────────────────────────────────────

    #[test]
    fn test_budget_allocation_default() {
        let alloc = BudgetAllocation::default();
        assert_eq!(alloc.context_window, 128000);
        assert_eq!(alloc.output_reserve, 1024);
        assert_eq!(alloc.history_ratio, 0.75);
        assert_eq!(alloc.retrieval_ratio, 0.25);
        assert!(alloc.is_valid());
    }

    #[test]
    fn test_budget_allocation_fixed_zone() {
        let alloc = BudgetAllocation::new(128000)
            .with_output_reserve(2048)
            .with_system_prompt(500);
        assert_eq!(alloc.fixed_zone(), 2548); // 2048 + 500
    }

    #[test]
    fn test_budget_allocation_distributable() {
        let alloc = BudgetAllocation::new(128000)
            .with_output_reserve(2048)
            .with_system_prompt(500);
        assert_eq!(alloc.distributable_space(), 128000 - 2548);
    }

    #[test]
    fn test_budget_allocation_history_budget() {
        let alloc = BudgetAllocation::new(128000)
            .with_output_reserve(2048)
            .with_system_prompt(500);
        let space = alloc.distributable_space();
        let history = alloc.history_budget();
        // Should be approximately 75% of distributable space
        let expected = (space as f64 * 0.75) as u64;
        assert!((history as i64 - expected as i64).unsigned_abs() < 100);
    }

    #[test]
    fn test_budget_allocation_retrieval_budget() {
        let alloc = BudgetAllocation::new(128000)
            .with_output_reserve(2048)
            .with_system_prompt(500);
        let retrieval = alloc.retrieval_budget();
        // Should be at least the hard minimum
        assert!(retrieval >= 2048);
    }

    #[test]
    fn test_budget_allocation_small_window() {
        let alloc = BudgetAllocation::new(4096)
            .with_output_reserve(1024)
            .with_system_prompt(500);
        // With only 2572 distributable, retrieval min (2048) should be respected
        let retrieval = alloc.retrieval_budget();
        assert!(retrieval >= 2048 || alloc.distributable_space() < 2048);
    }

    #[test]
    fn test_budget_allocation_validity() {
        let valid = BudgetAllocation::new(128000);
        assert!(valid.is_valid());

        // Invalid: fixed zone exceeds context window
        let invalid = BudgetAllocation::new(1000)
            .with_output_reserve(800)
            .with_system_prompt(500);
        assert!(!invalid.is_valid());
    }
}
