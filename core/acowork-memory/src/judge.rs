//! LLM Judge types for retrieval quality evaluation.
//!
//! ADR-051 P4: Moved from `acowork_grafeo::judge` to decouple Runtime
//! from the grafeo crate. These types have no grafeo dependencies.

/// Configuration for the LLM Judge.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeConfig {
    /// Model name used for judging (e.g., "qwen3:1.7b").
    pub model: String,
    /// Sampling rate [0.0, 1.0] - fraction of retrievals to evaluate.
    pub sample_rate: f32,
    /// Number of top results to evaluate per sample.
    pub top_k: usize,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            model: "qwen3:1.7b".to_string(),
            sample_rate: 0.1,
            top_k: 3,
        }
    }
}

/// Result of a single judgment.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeResult {
    /// Relevance score from 1 to 5.
    pub relevance_score: u8,
    /// Human-readable reasoning.
    pub reason: String,
}

/// Determine whether this retrieval should be sampled for judging.
///
/// Uses deterministic pseudo-random sampling based on `query_hash`
/// so the same query always produces the same decision.
pub fn should_sample(config: &JudgeConfig, query_hash: u64) -> bool {
    if config.sample_rate <= 0.0 {
        return false;
    }
    if config.sample_rate >= 1.0 {
        return true;
    }
    // Deterministic sampling using high 32 bits of a mixed hash
    // for uniform distribution across the full u64 space.
    let mixed = query_hash.wrapping_mul(0x9e3779b97f4a7c15);
    let threshold = (config.sample_rate * (u32::MAX as f32)) as u32;
    ((mixed >> 32) as u32) < threshold
}
