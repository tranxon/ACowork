//! Token counting module
//!
//! Provides tiered token counting with different precision levels:
//! - Tier 1: Exact counting (OpenAI → tiktoken-rs, Anthropic → official tokenizer), error < 1%
//! - Tier 2: Approximate counting (unknown models, first call remote, then ratio), error < 5%
//! - Tier 3: Heuristic estimation (English words×1.3, CJK chars×0.6), error < 15%

pub mod counter;

pub use counter::{TokenCounter, TokenCountTier, BudgetAllocation};
