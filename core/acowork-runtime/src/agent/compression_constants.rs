//! ADR-061 context compression - shared constants and budget validation.
//!
//! The 8-level degradation strategy (replacing "keep last N rounds") is
//! driven by these constants. Source of truth: docs/adr/zh/ADR-061 §19
//! (2026-08-30 finalized revision).

use acowork_core::protocol::ModelCapabilitiesInfo;

use crate::error::RuntimeError;

/// Minimum compression ratio for levels 1-7 (the target bar).
///
/// A level is only selected when it removes >= 10% of the history tokens —
/// below that the cache invalidation is not worth the summary cost
/// (ADR-061 §3.3 break-even analysis). Level 8 is exempt (see below).
pub(crate) const MIN_COMPRESSION_RATIO: f64 = 0.10;

/// Maximum output budget for the LLM compaction summary.
///
/// Replaces the former hardcoded `2048` in `compact_via_llm`. With the
/// mandatory three-section summary (`<summary>` / `<user_intent>` /
/// `<triples>`, ADR-061 §8.1) 2K is too tight; 4K leaves room for the
/// full user-intent list without truncating `</triples>`.
pub(crate) const SUMMARY_TOKEN_BUDGET: u64 = 4_096;

/// Model rejection line: agents require an effective input budget of at
/// least 64K to run the 8-level compression loop meaningfully.
///
/// Below this, level 8 (system + summary + current user) dominates the
/// context and the mechanism degenerates (ADR-061 §19.3). 128K/200K/1M
/// mainstream models all pass (128K - 32K output = 96K >= 64K).
pub(crate) const MIN_BUDGET_FOR_AGENT: u64 = 65_536;

/// Output-token reserve used when validating a model's effective input
/// budget (mirrors `max_output_tokens_limit`'s default of 32K).
pub(crate) const DEFAULT_OUTPUT_RESERVE: u64 = 32_768;

/// Validate that a model's effective input budget clears the agent
/// rejection line.
///
/// Called on `model_switch` (hard rejection) and session init (warning
/// only — see ADR-061 §13.4 implementation note).
pub(crate) fn validate_model_budget(
    caps: &ModelCapabilitiesInfo,
) -> std::result::Result<(), RuntimeError> {
    if caps.effective_input_budget(DEFAULT_OUTPUT_RESERVE) < MIN_BUDGET_FOR_AGENT {
        return Err(RuntimeError::UnsupportedModel(format!(
            "Model context window too small for agent loop: effective input budget {} < MIN_BUDGET_FOR_AGENT {}",
            caps.effective_input_budget(DEFAULT_OUTPUT_RESERVE),
            MIN_BUDGET_FOR_AGENT
        )));
    }
    Ok(())
}
