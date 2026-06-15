//! Token counting module
//!
//! Provides tiered token counting with different precision levels:
//! - Tier 1: Exact counting (OpenAI → tiktoken-rs, Anthropic → official tokenizer), error < 1%
//! - Tier 2: Approximate counting (unknown models, first call remote, then ratio), error < 5%
//! - Tier 3: Heuristic estimation (English words×1.3, CJK chars×0.6), error < 15%
//!
//! # Unified API
//!
//! **All** token counting in AgentCowork MUST go through [`count_text`].
//! Do NOT use `content.len() / 4` or any other ad-hoc heuristic —
//! they cause the debug panel and status panel to show contradictory numbers.
pub mod counter;

pub use counter::{TokenCountTier, TokenCounter, estimate_image_tokens};

/// The single unified entry point for token counting in AgentCowork.
///
/// Uses model-aware tiered counting:
/// - GPT models → tiktoken (Tier 1, < 1% error)
/// - Claude/Qwen/Llama → sampling ratio (Tier 2, < 5% error)
/// - Unknown models → word/CJK heuristic (Tier 3, < 15% error)
///
/// # Why a unified API matters
///
/// Before this function existed, token counting was scattered across:
/// - `content.len() / 4` in debug panel → overestimates Chinese text by ~2.9x
/// - `chars / 3.5` in context builder safety checks → inconsistent with debug
/// - `TokenCounter::count_text()` in history manager → the only correct path
///
/// Two different numbers displayed to the user for the same session is a UX bug.
/// This function ensures **one source of truth** for all token estimates.
pub fn count_text(text: &str, model: &str) -> usize {
    TokenCounter::new().count_text(text, model) as usize
}
