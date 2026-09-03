//! Decay score calculation.
//!
//! Implements the multiplicative decay model from design §5.1:
//!   decay_score = importance * activity_signal
//!   activity_signal = clamp(recency_boost + access_boost, FLOOR, 1.0)
//!   recency_boost = exp(-lambda * days_since_last_access)
//!   access_boost = min(BOOST_CAP, access_count * ACCESS_PER_HIT)
//!
//! The single source of truth for [`DecayConfig`] is `acowork_memory::types`,
//! which matches design §10.3 (7 fields: lambda, floor, access_per_hit,
//! boost_cap, dormant_threshold, purge_after, purge_importance_threshold).

pub use acowork_memory::types::DecayConfig;

/// Calculate multiplicative decay score (design §5.1).
///
/// Formula:
///   score = importance * activity_signal
///   activity_signal = clamp(recency_boost + access_boost, floor, 1.0)
///   recency_boost = exp(-lambda * days_since_last_access)
///   access_boost = min(boost_cap, access_per_hit * access_count)
///
/// The score is clamped to [0.0, 1.0].
pub fn compute_decay_score(
    config: &DecayConfig,
    importance: f32,
    days_since_last_access: f64,
    recent_access_count: u32,
) -> f32 {
    let recency = (-f64::from(config.lambda) * days_since_last_access).exp();
    let access = (f64::from(config.access_per_hit) * f64::from(recent_access_count))
        .min(f64::from(config.boost_cap));
    // FLOOR preserves a minimum activity so importance > 0 nodes never decay to 0.
    let activity_signal = (recency + access).clamp(f64::from(config.floor), 1.0);
    let score = f64::from(importance) * activity_signal;
    score.clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decay_score_fresh_node() {
        let config = DecayConfig::default();
        // Fresh node with high importance should have score close to importance.
        let score = compute_decay_score(&config, 0.9, 0.0, 0);
        assert!((score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn test_decay_score_old_node() {
        let config = DecayConfig::default();
        // 30 days old, no access, medium importance.
        let score = compute_decay_score(&config, 0.6, 30.0, 0);
        assert!(score < 0.6);
        assert!(score > 0.0);
        // Half-life check: after ~23 days recency ≈ 0.5, so with importance 1.0
        // and no access the score ≈ 0.5 * 1.0 = 0.5.
        let half_life_score = compute_decay_score(&config, 1.0, 23.0, 0);
        assert!((half_life_score - 0.5).abs() < 0.05);
    }

    #[test]
    fn test_decay_score_access_boost() {
        let config = DecayConfig::default();
        let score_no_access = compute_decay_score(&config, 0.5, 30.0, 0);
        let score_with_access = compute_decay_score(&config, 0.5, 30.0, 5);
        assert!(score_with_access > score_no_access);
    }

    #[test]
    fn test_decay_score_clamped() {
        let config = DecayConfig::default();
        // Score should never exceed 1.0.
        let score = compute_decay_score(&config, 1.0, 0.0, 100);
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decay_score_floor() {
        let config = DecayConfig::default();
        // Long-unused high-importance node must never decay below
        // importance * FLOOR (0.05), per design §5.1 FLOOR semantics.
        let score = compute_decay_score(&config, 1.0, 100_000.0, 0);
        assert!((score - 0.05).abs() < 1e-6, "score={score} should be FLOOR=0.05");
        // Zero importance still decays to 0 (FLOOR applies to activity, not score).
        let zero_importance = compute_decay_score(&config, 0.0, 100_000.0, 0);
        assert_eq!(zero_importance, 0.0);
    }

    #[test]
    fn test_decay_score_access_boost_capped() {
        let config = DecayConfig::default();
        // access_boost is capped at BOOST_CAP (0.5): 100 hits == 5 hits.
        let capped = compute_decay_score(&config, 1.0, 100_000.0, 100);
        let five_hits = compute_decay_score(&config, 1.0, 100_000.0, 5);
        assert!((capped - five_hits).abs() < 1e-6);
        // With recency≈0 and access capped at 0.5, activity = clamp(0.5, FLOOR, 1.0) = 0.5.
        assert!((capped - 0.5).abs() < 1e-6, "capped={capped} should be 0.5");
    }
}
