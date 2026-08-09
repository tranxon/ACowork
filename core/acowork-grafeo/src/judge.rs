//! Online LLM Judge for retrieval quality.
//!
//! ADR-051 P4: Types and functions moved to `acowork_memory::judge`.
//! This module re-exports them for backward compatibility.

pub use acowork_memory::judge::{JudgeConfig, JudgeResult, should_sample};

/// Evaluate retrieval quality (mock / placeholder for Phase 2).
///
/// In Phase 3 this will perform an actual LLM call. For now it returns
/// a fixed synthetic score for framework validation.
pub fn evaluate_retrieval(_config: &JudgeConfig, _query: &str, _results: &[String]) -> JudgeResult {
    JudgeResult {
        relevance_score: 4,
        reason: "Mock evaluation: results appear relevant to the query.".to_string(),
    }
}
