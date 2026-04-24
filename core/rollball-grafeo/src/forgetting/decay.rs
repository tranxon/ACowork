//! Decay score calculation.
//!
//! Implements the multiplicative decay model:
//!   decay_score = importance * activity_signal
//!   activity_signal = exp(-lambda * hours_since_last_access) + access_boost * recent_access_count

/// Decay configuration parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayConfig {
    /// Decay rate (default: 0.03).
    pub lambda: f64,
    /// Boost per access (default: 0.1).
    pub access_boost: f64,
    /// Score below which -> Dormant (default: 0.3).
    pub dormant_threshold: f32,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            lambda: 0.03,
            access_boost: 0.1,
            dormant_threshold: 0.3,
        }
    }
}

/// Calculate multiplicative decay score.
///
/// Formula:
///   score = importance * activity_signal
///   activity_signal = exp(-lambda * hours_since_last_access) + access_boost * recent_access_count
///
/// The score is clamped to [0.0, 1.0].
pub fn compute_decay_score(
    config: &DecayConfig,
    importance: f32,
    hours_since_last_access: f64,
    recent_access_count: u32,
) -> f32 {
    let recency = (-config.lambda * hours_since_last_access).exp();
    let access = config.access_boost * f64::from(recent_access_count);
    let activity_signal = (recency + access).min(1.0);
    let score = importance * activity_signal as f32;
    score.clamp(0.0, 1.0)
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
        let score = compute_decay_score(&config, 0.6, 24.0 * 30.0, 0);
        assert!(score < 0.6);
        assert!(score > 0.0);
    }

    #[test]
    fn test_decay_score_access_boost() {
        let config = DecayConfig::default();
        let score_no_access = compute_decay_score(&config, 0.5, 24.0 * 30.0, 0);
        let score_with_access = compute_decay_score(&config, 0.5, 24.0 * 30.0, 5);
        assert!(score_with_access > score_no_access);
    }

    #[test]
    fn test_decay_score_clamped() {
        let config = DecayConfig::default();
        // Score should never exceed 1.0.
        let score = compute_decay_score(&config, 1.0, 0.0, 100);
        assert!((score - 1.0).abs() < 1e-6);
    }
}
