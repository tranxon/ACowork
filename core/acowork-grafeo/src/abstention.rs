//! Abstention threshold mechanism.
//!
//! When retrieval results are below the confidence threshold, the Agent should
//! abstain from answering rather than hallucinating. This module provides the
//! configuration and logic for that decision.

/// Abstention configuration loaded from agent manifest.
#[derive(Debug, Clone)]
pub struct AbstentionConfig {
    /// Whether abstention is enabled. Default: true.
    pub enabled: bool,
    /// Default minimum score threshold. Default: 0.6.
    pub default_min_score: f32,
    /// Minimum score for tool-oriented agents. Default: 0.5.
    pub tool_agent_min_score: f32,
    /// Minimum score for learning-oriented agents. Default: 0.7.
    pub learning_agent_min_score: f32,
    /// Prompt injected when abstention triggers.
    pub abstention_prompt: String,
}

impl Default for AbstentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_min_score: 0.6,
            tool_agent_min_score: 0.5,
            learning_agent_min_score: 0.7,
            abstention_prompt: "When you are not confident about the information from memory, \
                respond with 'I'm not sure about this' rather than guessing."
                .to_string(),
        }
    }
}

/// Result of abstention check.
#[derive(Debug)]
pub struct AbstentionResult {
    /// Whether abstention was triggered.
    pub triggered: bool,
    /// Number of results before filtering.
    pub original_count: usize,
    /// Number of results after filtering (score >= threshold).
    pub filtered_count: usize,
    /// Highest score among original results.
    pub max_score: f32,
    /// The min_score threshold used for this check.
    pub threshold: f32,
    /// Prompt to inject into the System Prompt if abstention triggered.
    pub prompt_injection: Option<String>,
}

/// Check if abstention should be triggered and compute the result.
///
/// Takes a slice of raw relevance scores and an optional threshold override.
/// Results with scores below the effective threshold are considered filtered out.
/// Abstention is triggered when no results remain after filtering.
///
/// # Arguments
///
/// * `config` - Abstention configuration.
/// * `scores` - Raw relevance scores from retrieval (higher = more relevant).
/// * `min_score_override` - Optional threshold override. If `None`, uses
///   `config.default_min_score`.
pub fn check_abstention(
    config: &AbstentionConfig,
    scores: &[f64],
    min_score_override: Option<f32>,
) -> AbstentionResult {
    let threshold = min_score_override.unwrap_or(config.default_min_score);
    let original_count = scores.len();

    let max_score = scores.iter().map(|s| *s as f32).fold(0.0_f32, f32::max);

    if !config.enabled {
        return AbstentionResult {
            triggered: false,
            original_count,
            filtered_count: original_count,
            max_score,
            threshold,
            prompt_injection: None,
        };
    }

    let filtered_count = scores
        .iter()
        .filter(|s| **s >= f64::from(threshold))
        .count();

    let triggered = filtered_count == 0 && original_count > 0;

    let prompt_injection = if triggered {
        Some(config.abstention_prompt.clone())
    } else {
        None
    };

    AbstentionResult {
        triggered,
        original_count,
        filtered_count,
        max_score,
        threshold,
        prompt_injection,
    }
}

/// Get the appropriate min_score for an agent type.
///
/// # Arguments
///
/// * `config` - Abstention configuration.
/// * `agent_type` - Agent type string. Recognized values: `"tool"`, `"learning"`.
///   Any other value falls back to `default_min_score`.
pub fn get_min_score_for_agent(config: &AbstentionConfig, agent_type: &str) -> f32 {
    match agent_type {
        "tool" => config.tool_agent_min_score,
        "learning" => config.learning_agent_min_score,
        _ => config.default_min_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Test 1: Default configuration values
    // =====================================================================

    #[test]
    fn test_abstention_config_default() {
        let config = AbstentionConfig::default();
        assert!(config.enabled);
        assert_eq!(config.default_min_score, 0.6);
        assert_eq!(config.tool_agent_min_score, 0.5);
        assert_eq!(config.learning_agent_min_score, 0.7);
        assert!(!config.abstention_prompt.is_empty());
    }

    // =====================================================================
    // Test 2: Abstention is triggered when all scores are below threshold
    // =====================================================================

    #[test]
    fn test_abstention_triggered() {
        let config = AbstentionConfig::default();
        let scores = vec![0.3_f64, 0.4, 0.5];

        let result = check_abstention(&config, &scores, None);

        assert!(result.triggered);
        assert_eq!(result.original_count, 3);
        assert_eq!(result.filtered_count, 0);
        assert_eq!(result.threshold, 0.6);
        assert!(result.prompt_injection.is_some());
    }

    // =====================================================================
    // Test 3: Abstention is not triggered when at least one score passes
    // =====================================================================

    #[test]
    fn test_abstention_not_triggered() {
        let config = AbstentionConfig::default();
        let scores = vec![0.3_f64, 0.7, 0.5];

        let result = check_abstention(&config, &scores, None);

        assert!(!result.triggered);
        assert_eq!(result.original_count, 3);
        assert_eq!(result.filtered_count, 1);
        assert!(result.prompt_injection.is_none());
    }

    // =====================================================================
    // Test 4: Agent type score selection
    // =====================================================================

    #[test]
    fn test_get_min_score_for_agent() {
        let config = AbstentionConfig::default();

        assert_eq!(get_min_score_for_agent(&config, "tool"), 0.5);
        assert_eq!(get_min_score_for_agent(&config, "learning"), 0.7);
        assert_eq!(get_min_score_for_agent(&config, "default"), 0.6);
        assert_eq!(get_min_score_for_agent(&config, "unknown"), 0.6);
        assert_eq!(get_min_score_for_agent(&config, ""), 0.6);
    }

    // =====================================================================
    // Test 5: Prompt injection content when abstention triggers
    // =====================================================================

    #[test]
    fn test_abstention_prompt_injection() {
        let config = AbstentionConfig::default();
        let scores = vec![0.3_f64, 0.4];

        let result = check_abstention(&config, &scores, None);

        assert!(result.triggered);
        let prompt = result.prompt_injection.unwrap();
        assert!(prompt.contains("not sure about this"));
        assert!(prompt.contains("guessing"));
    }

    // =====================================================================
    // Test 6: min_score override works correctly
    // =====================================================================

    #[test]
    fn test_abstention_with_override() {
        let config = AbstentionConfig::default();
        // 0.55 is below default 0.6 but above override 0.5
        let scores = vec![0.55_f64];

        let result = check_abstention(&config, &scores, Some(0.5));

        assert!(!result.triggered);
        assert_eq!(result.filtered_count, 1);
        assert_eq!(result.threshold, 0.5);
    }

    // =====================================================================
    // Test 7: Disabled abstention never triggers
    // =====================================================================

    #[test]
    fn test_abstention_disabled() {
        let config = AbstentionConfig {
            enabled: false,
            ..AbstentionConfig::default()
        };
        let scores = vec![0.3_f64];

        let result = check_abstention(&config, &scores, None);

        assert!(!result.triggered);
        assert_eq!(result.original_count, 1);
        assert_eq!(result.filtered_count, 1);
        assert!(result.prompt_injection.is_none());
    }

    // =====================================================================
    // Test 8: Empty scores — abstention is not triggered
    // =====================================================================

    #[test]
    fn test_abstention_empty_scores() {
        let config = AbstentionConfig::default();
        let scores: Vec<f64> = vec![];

        let result = check_abstention(&config, &scores, None);

        assert!(!result.triggered);
        assert_eq!(result.original_count, 0);
        assert_eq!(result.filtered_count, 0);
        assert_eq!(result.max_score, 0.0);
    }

    // =====================================================================
    // Test 9: max_score reflects the highest original score
    // =====================================================================

    #[test]
    fn test_abstention_max_score() {
        let config = AbstentionConfig::default();
        let scores = vec![0.3_f64, 0.8, 0.5];

        let result = check_abstention(&config, &scores, None);

        assert!(!result.triggered);
        assert_eq!(result.max_score, 0.8);
    }
}
