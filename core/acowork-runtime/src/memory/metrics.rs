//! Runtime-internal retrieval quality metrics aggregator.
//!
//! Replaces the previous dependency on `acowork_grafeo::retrieval_metrics::MetricsAggregator`.
//! Data sources are `acowork_memory::RetrievalMetrics` and `acowork_memory::HintType`,
//! eliminating the HintType conversion code that previously coupled Runtime to Grafeo.
//!
//! Design ref: ADR-051 §5.2 - MetricsAggregator stays in Runtime (observation layer),
//! data comes from Provider return values.

use std::collections::VecDeque;

use acowork_memory::RetrievalMetrics;

// ---------------------------------------------------------------------------
// Alert types
// ---------------------------------------------------------------------------

/// Alert thresholds for metrics monitoring.
#[derive(Debug, Clone)]
pub struct AlertThresholds {
    /// NRR below this triggers a warning. Default: 0.5.
    pub nrr_warning: f32,
    /// Number of consecutive low-NRR retrievals before alerting. Default: 10.
    pub nrr_consecutive_limit: usize,
    /// Abstention rate above this triggers a warning. Default: 0.3.
    pub abstention_rate_high: f32,
    /// Abstention rate below this triggers a warning. Default: 0.05.
    pub abstention_rate_low: f32,
    /// Conflict accuracy below this triggers fallback to LLM arbitration. Default: 0.8.
    pub conflict_accuracy_min: f32,
    /// Degradation level 2+ frequency above this triggers a warning. Default: 0.2.
    pub degradation_rate_high: f32,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            nrr_warning: 0.5,
            nrr_consecutive_limit: 10,
            abstention_rate_high: 0.3,
            abstention_rate_low: 0.05,
            conflict_accuracy_min: 0.8,
            degradation_rate_high: 0.2,
        }
    }
}

/// Types of metrics alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricsAlertType {
    /// NRR is consistently low.
    LowNrr,
    /// Abstention rate is too high.
    HighAbstentionRate,
    /// Abstention rate is too low.
    LowAbstentionRate,
    /// Conflict resolution accuracy is below threshold.
    LowConflictAccuracy,
    /// Too many retrievals are in degraded mode.
    HighDegradationRate,
}

/// An alert triggered by metrics monitoring.
#[derive(Debug, Clone)]
pub struct MetricsAlert {
    /// Type of alert.
    pub alert_type: MetricsAlertType,
    /// Human-readable description.
    pub message: String,
    /// The metric value that triggered the alert.
    pub value: f32,
    /// The threshold that was crossed.
    pub threshold: f32,
}

// ---------------------------------------------------------------------------
// Conflict resolution accuracy tracking
// ---------------------------------------------------------------------------

/// Record of a conflict resolution decision.
#[derive(Debug, Clone)]
pub struct ConflictResolutionRecord {
    /// The heuristic classification (Evolution, Correction, Ambiguous).
    pub heuristic_type: String,
    /// The final resolution (may differ for Ambiguous -> LLM/user arbitration).
    pub final_type: String,
    /// Whether the heuristic matched the final resolution.
    pub correct: bool,
    /// Whether this was auto-resolved or required arbitration.
    pub auto_resolved: bool,
}

/// Accuracy statistics for conflict resolution.
#[derive(Debug, Clone, Default)]
pub struct ConflictAccuracyStats {
    /// Total number of conflict resolutions.
    pub total: usize,
    /// Number where the heuristic matched the final resolution.
    pub correct: usize,
    /// Number that were auto-resolved (no arbitration needed).
    pub auto_resolved: usize,
    /// Number that required LLM or user arbitration.
    pub arbitrated: usize,
}

impl ConflictAccuracyStats {
    /// Compute the auto-resolution accuracy rate.
    pub fn accuracy(&self) -> f32 {
        if self.total == 0 {
            return 1.0; // No conflicts -> perfect by default
        }
        self.correct as f32 / self.total as f32
    }

    /// Compute the auto-resolution rate (fraction resolved without arbitration).
    pub fn auto_resolution_rate(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        self.auto_resolved as f32 / self.total as f32
    }

    /// Record a new conflict resolution.
    pub fn record(&mut self, record: &ConflictResolutionRecord) {
        self.total += 1;
        if record.correct {
            self.correct += 1;
        }
        if record.auto_resolved {
            self.auto_resolved += 1;
        } else {
            self.arbitrated += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// RetrievalMetricsAggregator
// ---------------------------------------------------------------------------

/// Runtime-internal retrieval quality metrics aggregator.
///
/// Data source: `acowork_memory::RetrievalMetrics` returned by `MemoryProvider::retrieve()`.
/// Does NOT depend on `acowork_grafeo::retrieval_metrics` types.
///
/// Tracks:
/// - NRR (Normalized Retrieval Relevance) sliding window
/// - Abstention rate
/// - Degradation level frequency
/// - Conflict resolution accuracy
/// - LLM Judge relevance scores
///
/// Design ref: ADR-051 §5.2
#[derive(Debug)]
pub struct RetrievalMetricsAggregator {
    /// Recent NRR values (sliding window).
    nrr_history: VecDeque<f32>,
    /// Total retrievals tracked.
    total_retrievals: usize,
    /// Number of abstentions triggered.
    abstention_count: usize,
    /// Number of retrievals at degradation level 2+.
    high_degradation_count: usize,
    /// Maximum possible score for NRR computation.
    max_possible_score: f32,
    /// Conflict resolution accuracy tracker.
    conflict_stats: ConflictAccuracyStats,
    /// LLM Judge relevance scores (normalized 0.0-1.0, sliding window).
    judge_score_history: VecDeque<f32>,
    /// Number of LLM Judge evaluations performed.
    judge_eval_count: usize,
    /// Alert thresholds.
    thresholds: AlertThresholds,
    /// Size of the NRR sliding window.
    window_size: usize,
}

impl RetrievalMetricsAggregator {
    /// Create a new aggregator with the given max possible score and thresholds.
    pub fn new(max_possible_score: f32, thresholds: AlertThresholds) -> Self {
        Self {
            nrr_history: VecDeque::with_capacity(100),
            total_retrievals: 0,
            abstention_count: 0,
            high_degradation_count: 0,
            max_possible_score,
            conflict_stats: ConflictAccuracyStats::default(),
            judge_score_history: VecDeque::with_capacity(100),
            judge_eval_count: 0,
            thresholds,
            window_size: 100,
        }
    }

    /// Create a new aggregator with default settings.
    pub fn with_defaults(max_possible_score: f32) -> Self {
        Self::new(max_possible_score, AlertThresholds::default())
    }

    /// Get the current max_possible_score used for NRR computation.
    pub fn max_possible_score(&self) -> f32 {
        self.max_possible_score
    }

    /// Update the max_possible_score (e.g., when a higher score is observed).
    pub fn set_max_possible_score(&mut self, score: f32) {
        self.max_possible_score = score;
    }

    /// Compute NRR from a RetrievalMetrics value.
    ///
    /// NRR = avg_score / max_possible_score
    fn nrr(&self, metrics: &RetrievalMetrics) -> f32 {
        if self.max_possible_score <= 0.0 {
            return 0.0;
        }
        (metrics.avg_score / self.max_possible_score).clamp(0.0, 1.0)
    }

    /// Record a retrieval operation's metrics.
    ///
    /// Accepts `acowork_memory::RetrievalMetrics` directly - no HintType
    /// conversion needed (unlike the old grafeo-based code).
    ///
    /// Returns any alerts triggered by this observation.
    pub fn record_retrieval(&mut self, metrics: &RetrievalMetrics) -> Vec<MetricsAlert> {
        let mut alerts = Vec::new();

        self.total_retrievals += 1;

        // Track NRR
        let nrr = self.nrr(metrics);
        self.nrr_history.push_back(nrr);
        if self.nrr_history.len() > self.window_size {
            self.nrr_history.pop_front();
        }

        // Check consecutive low NRR
        let consecutive_low = self
            .nrr_history
            .iter()
            .rev()
            .take_while(|&&v| v < self.thresholds.nrr_warning)
            .count();
        if consecutive_low >= self.thresholds.nrr_consecutive_limit {
            alerts.push(MetricsAlert {
                alert_type: MetricsAlertType::LowNrr,
                message: format!(
                    "NRR below {} for {} consecutive retrievals - check embedding model or index",
                    self.thresholds.nrr_warning, consecutive_low
                ),
                value: nrr,
                threshold: self.thresholds.nrr_warning,
            });
        }

        // Track abstention
        if metrics.abstention_triggered {
            self.abstention_count += 1;
        }
        let abstention_rate = self.abstention_count as f32 / self.total_retrievals as f32;
        if abstention_rate > self.thresholds.abstention_rate_high {
            alerts.push(MetricsAlert {
                alert_type: MetricsAlertType::HighAbstentionRate,
                message: format!(
                    "Abstention rate {:.1}% exceeds {:.1}% - consider lowering min_score",
                    abstention_rate * 100.0,
                    self.thresholds.abstention_rate_high * 100.0,
                ),
                value: abstention_rate,
                threshold: self.thresholds.abstention_rate_high,
            });
        } else if abstention_rate < self.thresholds.abstention_rate_low
            && self.total_retrievals >= 20
        {
            alerts.push(MetricsAlert {
                alert_type: MetricsAlertType::LowAbstentionRate,
                message: format!(
                    "Abstention rate {:.1}% below {:.1}% - min_score may be too low",
                    abstention_rate * 100.0,
                    self.thresholds.abstention_rate_low * 100.0,
                ),
                value: abstention_rate,
                threshold: self.thresholds.abstention_rate_low,
            });
        }

        // Track degradation
        if metrics.retrieval_level >= 2 {
            self.high_degradation_count += 1;
        }
        let degradation_rate = self.high_degradation_count as f32 / self.total_retrievals as f32;
        if degradation_rate > self.thresholds.degradation_rate_high && self.total_retrievals >= 10 {
            alerts.push(MetricsAlert {
                alert_type: MetricsAlertType::HighDegradationRate,
                message: format!(
                    "Degradation level 2+ rate {:.1}% exceeds {:.1}% - check memory provider health",
                    degradation_rate * 100.0,
                    self.thresholds.degradation_rate_high * 100.0,
                ),
                value: degradation_rate,
                threshold: self.thresholds.degradation_rate_high,
            });
        }

        alerts
    }

    /// Record a conflict resolution decision.
    /// Returns an alert if accuracy drops below threshold.
    pub fn record_conflict(&mut self, record: &ConflictResolutionRecord) -> Option<MetricsAlert> {
        self.conflict_stats.record(record);

        if self.conflict_stats.total >= 5 {
            let accuracy = self.conflict_stats.accuracy();
            if accuracy < self.thresholds.conflict_accuracy_min {
                return Some(MetricsAlert {
                    alert_type: MetricsAlertType::LowConflictAccuracy,
                    message: format!(
                        "Conflict accuracy {:.1}% below {:.1}% - fallback to LLM arbitration",
                        accuracy * 100.0,
                        self.thresholds.conflict_accuracy_min * 100.0,
                    ),
                    value: accuracy,
                    threshold: self.thresholds.conflict_accuracy_min,
                });
            }
        }

        None
    }

    /// Get the current NRR (average over the sliding window).
    pub fn current_nrr(&self) -> f32 {
        if self.nrr_history.is_empty() {
            return 1.0;
        }
        self.nrr_history.iter().sum::<f32>() / self.nrr_history.len() as f32
    }

    /// Get the current abstention rate.
    pub fn abstention_rate(&self) -> f32 {
        if self.total_retrievals == 0 {
            return 0.0;
        }
        self.abstention_count as f32 / self.total_retrievals as f32
    }

    /// Get the current degradation rate.
    pub fn degradation_rate(&self) -> f32 {
        if self.total_retrievals == 0 {
            return 0.0;
        }
        self.high_degradation_count as f32 / self.total_retrievals as f32
    }

    /// Get the conflict accuracy stats.
    pub fn conflict_stats(&self) -> &ConflictAccuracyStats {
        &self.conflict_stats
    }

    /// Get the total number of retrievals tracked.
    pub fn total_retrievals(&self) -> usize {
        self.total_retrievals
    }

    /// Record a LLM Judge evaluation score.
    ///
    /// Called from the background Judge evaluation (10% sampling).
    /// The score is on a 1–5 scale (5 = highly relevant).
    /// Results are tracked in a sliding window for trend analysis.
    pub fn record_judge_score(&mut self, score: u8) {
        let normalized = (score as f32 / 5.0).clamp(0.0, 1.0);
        self.judge_score_history.push_back(normalized);
        if self.judge_score_history.len() > self.window_size {
            self.judge_score_history.pop_front();
        }
        self.judge_eval_count += 1;
    }

    /// Get the average LLM Judge score (normalized to 0.0–1.0).
    ///
    /// Returns 1.0 if no evaluations have been performed (optimistic default).
    pub fn avg_judge_score(&self) -> f32 {
        if self.judge_score_history.is_empty() {
            return 1.0;
        }
        self.judge_score_history.iter().sum::<f32>() / self.judge_score_history.len() as f32
    }

    /// Get the total number of LLM Judge evaluations performed.
    pub fn judge_eval_count(&self) -> usize {
        self.judge_eval_count
    }
}

// Note: HintType from acowork_memory is used implicitly through RetrievalMetrics.
// No explicit HintType import needed - the aggregator accepts RetrievalMetrics directly.
