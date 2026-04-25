//! Retrieval quality metrics — online evaluation and offline benchmarking.
//!
//! Phase 3 S4.5: Quantitative evaluation of memory retrieval quality.
//!
//! Two complementary dimensions (per docs/05-memory.md §11):
//! - **Online evaluation** (Runtime): metrics collected after each retrieval,
//!   including result counts, scores, abstention triggers, and degradation level.
//! - **Offline benchmarking**: precision@k, recall@k, MRR for controlled
//!   evaluation against ground truth.
//!
//! Additional capabilities:
//! - NRR (Normalized Retrieval Relevance)
//! - Conflict resolution accuracy tracking
//! - Metrics aggregation with configurable alert thresholds

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Online retrieval metrics (per retrieval operation)
// ---------------------------------------------------------------------------

/// Hint type that triggered the retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HintType {
    /// Semantic search (vector similarity).
    Semantic,
    /// Full-text search (BM25).
    FullText,
    /// RRF hybrid (vector + BM25).
    Hybrid,
    /// Graph expansion (spreading activation).
    GraphExpand,
}

impl HintType {
    /// Short code used in retrieval analysis.
    pub fn code(&self) -> &'static str {
        match self {
            HintType::Semantic => "s",
            HintType::FullText => "f",
            HintType::Hybrid => "r",
            HintType::GraphExpand => "i",
        }
    }
}

/// Metrics collected after each retrieval operation.
/// Computed asynchronously to avoid impacting retrieval latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineRetrievalMetrics {
    /// Number of results returned.
    pub result_count: usize,
    /// Average relevance score of results.
    pub avg_score: f32,
    /// Highest relevance score.
    pub max_score: f32,
    /// Whether Abstention was triggered (§6.5).
    pub abstention_triggered: bool,
    /// Degradation level (0-3, §6.1).
    pub retrieval_level: u8,
    /// Number of nodes expanded via graph_expand.
    pub graph_expand_nodes: usize,
    /// Hint type used for this retrieval.
    pub hint_type: HintType,
}

impl OnlineRetrievalMetrics {
    /// Compute NRR (Normalized Retrieval Relevance).
    ///
    /// NRR = avg_score / max_possible_score
    /// NRR < 0.5 → retrieval quality warning
    /// NRR > 0.8 → retrieval quality is good
    pub fn nrr(&self, max_possible_score: f32) -> f32 {
        if max_possible_score <= 0.0 {
            return 0.0;
        }
        (self.avg_score / max_possible_score).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Conflict resolution accuracy tracking
// ---------------------------------------------------------------------------

/// Record of a conflict resolution decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResolutionRecord {
    /// The heuristic classification (Evolution, Correction, Ambiguous).
    pub heuristic_type: String,
    /// The final resolution (may differ for Ambiguous → LLM/user arbitration).
    pub final_type: String,
    /// Whether the heuristic matched the final resolution.
    pub correct: bool,
    /// Whether this was auto-resolved or required arbitration.
    pub auto_resolved: bool,
}

/// Accuracy statistics for conflict resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
            return 1.0; // No conflicts → perfect by default
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
// Metrics aggregator (trend tracking and alerting)
// ---------------------------------------------------------------------------

/// Alert thresholds for metrics monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// An alert triggered by metrics monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Types of metrics alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Aggregator that tracks metrics trends and triggers alerts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsAggregator {
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
    /// Alert thresholds.
    thresholds: AlertThresholds,
    /// Size of the NRR sliding window.
    window_size: usize,
}

impl MetricsAggregator {
    /// Create a new aggregator with the given max possible score and thresholds.
    pub fn new(max_possible_score: f32, thresholds: AlertThresholds) -> Self {
        Self {
            nrr_history: VecDeque::with_capacity(100),
            total_retrievals: 0,
            abstention_count: 0,
            high_degradation_count: 0,
            max_possible_score,
            conflict_stats: ConflictAccuracyStats::default(),
            thresholds,
            window_size: 100,
        }
    }

    /// Create a new aggregator with default settings.
    pub fn with_defaults(max_possible_score: f32) -> Self {
        Self::new(max_possible_score, AlertThresholds::default())
    }

    /// Record a retrieval operation's metrics.
    /// Returns any alerts triggered by this observation.
    pub fn record_retrieval(&mut self, metrics: &OnlineRetrievalMetrics) -> Vec<MetricsAlert> {
        let mut alerts = Vec::new();

        self.total_retrievals += 1;

        // Track NRR
        let nrr = metrics.nrr(self.max_possible_score);
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
                    "NRR below {} for {} consecutive retrievals — check embedding model or index",
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
                    "Abstention rate {:.1}% exceeds {:.1}% — consider lowering min_score",
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
                    "Abstention rate {:.1}% below {:.1}% — min_score may be too low",
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
                    "Degradation level 2+ rate {:.1}% exceeds {:.1}% — check Grafeo health",
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
                        "Conflict accuracy {:.1}% below {:.1}% — fallback to LLM arbitration",
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
}

// ---------------------------------------------------------------------------
// Offline benchmarking metrics
// ---------------------------------------------------------------------------

/// Result of an offline benchmark evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    /// Precision at k — fraction of retrieved items that are relevant.
    pub precision_at_k: Vec<(usize, f32)>,
    /// Recall at k — fraction of relevant items that are retrieved.
    pub recall_at_k: Vec<(usize, f32)>,
    /// Mean Reciprocal Rank — average of 1/rank of first relevant result.
    pub mrr: f32,
    /// Number of queries evaluated.
    pub num_queries: usize,
}

/// A single evaluation query with known relevant results.
#[derive(Debug, Clone)]
pub struct EvalQuery {
    /// The query text.
    pub query: String,
    /// IDs of relevant knowledge nodes (ground truth).
    pub relevant_ids: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Metrics computation
// ---------------------------------------------------------------------------

/// Compute precision@k for a ranked list of results.
///
/// Precision@k = |relevant ∩ retrieved[:k]| / k
pub fn precision_at_k(retrieved: &[u64], relevant: &[u64], k: usize) -> f32 {
    if k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let actual_k = k.min(retrieved.len());
    let relevant_set: std::collections::HashSet<u64> = relevant.iter().copied().collect();
    let hits = retrieved[..actual_k]
        .iter()
        .filter(|id| relevant_set.contains(id))
        .count();
    hits as f32 / actual_k as f32
}

/// Compute recall@k for a ranked list of results.
///
/// Recall@k = |relevant ∩ retrieved[:k]| / |relevant|
pub fn recall_at_k(retrieved: &[u64], relevant: &[u64], k: usize) -> f32 {
    if relevant.is_empty() {
        return 1.0; // No relevant items → perfect recall
    }
    let actual_k = k.min(retrieved.len());
    let relevant_set: std::collections::HashSet<u64> = relevant.iter().copied().collect();
    let hits = retrieved[..actual_k]
        .iter()
        .filter(|id| relevant_set.contains(id))
        .count();
    hits as f32 / relevant.len() as f32
}

/// Compute Mean Reciprocal Rank (MRR) across multiple queries.
///
/// MRR = average(1/rank_i) where rank_i is the rank of the first
/// relevant result for query i.
pub fn mean_reciprocal_rank(
    queries: &[(Vec<u64>, Vec<u64>)], // (retrieved, relevant) per query
) -> f32 {
    if queries.is_empty() {
        return 0.0;
    }

    let mut total_rr = 0.0;
    for (retrieved, relevant) in queries {
        let relevant_set: std::collections::HashSet<u64> = relevant.iter().copied().collect();
        let rr = retrieved
            .iter()
            .position(|id| relevant_set.contains(id))
            .map(|pos| 1.0 / (pos + 1) as f32)
            .unwrap_or(0.0);
        total_rr += rr;
    }

    total_rr / queries.len() as f32
}

/// Evaluate retrieval quality across multiple queries.
pub fn evaluate_retrieval_quality(
    queries: &[EvalQuery],
    retrieval_results: &[Vec<u64>], // retrieved node IDs per query
    k_values: &[usize],
) -> BenchmarkMetrics {
    let mut precision_results = Vec::new();
    let mut recall_results = Vec::new();

    for &k in k_values {
        let mut total_precision = 0.0;
        let mut total_recall = 0.0;

        for (query, retrieved) in queries.iter().zip(retrieval_results.iter()) {
            total_precision += precision_at_k(retrieved, &query.relevant_ids, k);
            total_recall += recall_at_k(retrieved, &query.relevant_ids, k);
        }

        let n = queries.len() as f32;
        precision_results.push((k, total_precision / n));
        recall_results.push((k, total_recall / n));
    }

    let mrr_queries: Vec<(Vec<u64>, Vec<u64>)> = queries
        .iter()
        .zip(retrieval_results.iter())
        .map(|(q, r)| (r.clone(), q.relevant_ids.clone()))
        .collect();

    let mrr = mean_reciprocal_rank(&mrr_queries);

    BenchmarkMetrics {
        precision_at_k: precision_results,
        recall_at_k: recall_results,
        mrr,
        num_queries: queries.len(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // =====================================================================
    // Test: precision@k
    // =====================================================================

    #[test]
    fn test_precision_at_k_perfect() {
        let retrieved = vec![1, 2, 3, 4, 5];
        let relevant = vec![1, 2, 3];
        assert!((precision_at_k(&retrieved, &relevant, 3) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_precision_at_k_partial() {
        let retrieved = vec![1, 6, 3, 7, 8];
        let relevant = vec![1, 2, 3];
        let p = precision_at_k(&retrieved, &relevant, 3);
        assert!((p - 0.667).abs() < 0.01);
    }

    #[test]
    fn test_precision_at_k_empty() {
        let retrieved: Vec<u64> = vec![];
        let relevant = vec![1, 2];
        assert!((precision_at_k(&retrieved, &relevant, 3)).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: recall@k
    // =====================================================================

    #[test]
    fn test_recall_at_k_perfect() {
        let retrieved = vec![1, 2, 3, 4, 5];
        let relevant = vec![1, 2, 3];
        assert!((recall_at_k(&retrieved, &relevant, 5) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recall_at_k_partial() {
        let retrieved = vec![1, 6, 7];
        let relevant = vec![1, 2, 3];
        let r = recall_at_k(&retrieved, &relevant, 3);
        assert!((r - 0.333).abs() < 0.01);
    }

    #[test]
    fn test_recall_at_k_no_relevant() {
        let retrieved = vec![1, 2];
        let relevant: Vec<u64> = vec![];
        assert!((recall_at_k(&retrieved, &relevant, 3) - 1.0).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: MRR
    // =====================================================================

    #[test]
    fn test_mrr_perfect() {
        let queries = vec![
            (vec![1, 2, 3], vec![1, 4]),
            (vec![5, 2, 3], vec![2, 6]),
        ];
        let mrr = mean_reciprocal_rank(&queries);
        assert!((mrr - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_mrr_no_hits() {
        let queries = vec![
            (vec![5, 6, 7], vec![1, 2]),
        ];
        let mrr = mean_reciprocal_rank(&queries);
        assert!(mrr.abs() < f32::EPSILON);
    }

    #[test]
    fn test_mrr_empty() {
        let mrr = mean_reciprocal_rank(&[]);
        assert!(mrr.abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: evaluate_retrieval_quality
    // =====================================================================

    #[test]
    fn test_evaluate_retrieval_quality() {
        let queries = vec![
            EvalQuery {
                query: "weather".to_string(),
                relevant_ids: vec![1, 2],
            },
            EvalQuery {
                query: "calendar".to_string(),
                relevant_ids: vec![3, 4],
            },
        ];

        let results = vec![
            vec![1, 5, 2],
            vec![3, 6, 4],
        ];

        let metrics = evaluate_retrieval_quality(&queries, &results, &[1, 3, 5]);

        assert_eq!(metrics.num_queries, 2);
        assert_eq!(metrics.precision_at_k.len(), 3);
        assert_eq!(metrics.recall_at_k.len(), 3);
        assert!((metrics.precision_at_k[0].1 - 1.0).abs() < f32::EPSILON);
        assert!((metrics.mrr - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_benchmark_metrics_serde() {
        let metrics = BenchmarkMetrics {
            precision_at_k: vec![(1, 0.8), (3, 0.6)],
            recall_at_k: vec![(1, 0.4), (3, 0.7)],
            mrr: 0.75,
            num_queries: 10,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let decoded: BenchmarkMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(metrics.num_queries, decoded.num_queries);
        assert!((metrics.mrr - decoded.mrr).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: OnlineRetrievalMetrics
    // =====================================================================

    #[test]
    fn test_online_metrics_nrr() {
        let metrics = OnlineRetrievalMetrics {
            result_count: 5,
            avg_score: 0.7,
            max_score: 0.95,
            abstention_triggered: false,
            retrieval_level: 0,
            graph_expand_nodes: 3,
            hint_type: HintType::Hybrid,
        };
        let nrr = metrics.nrr(1.0);
        assert!((nrr - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_online_metrics_nrr_zero_max() {
        let metrics = OnlineRetrievalMetrics {
            result_count: 0,
            avg_score: 0.0,
            max_score: 0.0,
            abstention_triggered: true,
            retrieval_level: 3,
            graph_expand_nodes: 0,
            hint_type: HintType::Semantic,
        };
        let nrr = metrics.nrr(0.0);
        assert!((nrr - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_online_metrics_serde() {
        let metrics = OnlineRetrievalMetrics {
            result_count: 3,
            avg_score: 0.8,
            max_score: 0.95,
            abstention_triggered: false,
            retrieval_level: 1,
            graph_expand_nodes: 5,
            hint_type: HintType::GraphExpand,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let decoded: OnlineRetrievalMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(metrics.result_count, decoded.result_count);
        assert_eq!(metrics.hint_type, decoded.hint_type);
    }

    // =====================================================================
    // Test: HintType codes
    // =====================================================================

    #[test]
    fn test_hint_type_codes() {
        assert_eq!(HintType::Semantic.code(), "s");
        assert_eq!(HintType::FullText.code(), "f");
        assert_eq!(HintType::Hybrid.code(), "r");
        assert_eq!(HintType::GraphExpand.code(), "i");
    }

    // =====================================================================
    // Test: ConflictAccuracyStats
    // =====================================================================

    #[test]
    fn test_conflict_accuracy_empty() {
        let stats = ConflictAccuracyStats::default();
        assert!((stats.accuracy() - 1.0).abs() < f32::EPSILON);
        assert!((stats.auto_resolution_rate() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_conflict_accuracy_with_records() {
        let mut stats = ConflictAccuracyStats::default();

        // Record 5 correct auto-resolutions
        for _ in 0..5 {
            stats.record(&ConflictResolutionRecord {
                heuristic_type: "Evolution".to_string(),
                final_type: "Evolution".to_string(),
                correct: true,
                auto_resolved: true,
            });
        }

        // Record 2 incorrect (needs arbitration)
        stats.record(&ConflictResolutionRecord {
            heuristic_type: "Evolution".to_string(),
            final_type: "Correction".to_string(),
            correct: false,
            auto_resolved: false,
        });
        stats.record(&ConflictResolutionRecord {
            heuristic_type: "Ambiguous".to_string(),
            final_type: "Evolution".to_string(),
            correct: false,
            auto_resolved: false,
        });

        assert_eq!(stats.total, 7);
        assert_eq!(stats.correct, 5);
        assert_eq!(stats.auto_resolved, 5);
        assert_eq!(stats.arbitrated, 2);
        assert!((stats.accuracy() - 5.0 / 7.0).abs() < 0.01);
        assert!((stats.auto_resolution_rate() - 5.0 / 7.0).abs() < 0.01);
    }

    // =====================================================================
    // Test: MetricsAggregator — basic tracking
    // =====================================================================

    #[test]
    fn test_aggregator_basic_tracking() {
        let mut agg = MetricsAggregator::with_defaults(1.0);

        let metrics = OnlineRetrievalMetrics {
            result_count: 5,
            avg_score: 0.8,
            max_score: 0.95,
            abstention_triggered: false,
            retrieval_level: 0,
            graph_expand_nodes: 3,
            hint_type: HintType::Hybrid,
        };

        let alerts = agg.record_retrieval(&metrics);
        assert!(alerts.is_empty());
        assert_eq!(agg.total_retrievals(), 1);
        assert!((agg.current_nrr() - 0.8).abs() < f32::EPSILON);
        assert!((agg.abstention_rate() - 0.0).abs() < f32::EPSILON);
    }

    // =====================================================================
    // Test: MetricsAggregator — consecutive low NRR alert
    // =====================================================================

    #[test]
    fn test_aggregator_low_nrr_alert() {
        let mut agg = MetricsAggregator::with_defaults(1.0);

        let low_nrr_metrics = OnlineRetrievalMetrics {
            result_count: 5,
            avg_score: 0.3,
            max_score: 0.5,
            abstention_triggered: false,
            retrieval_level: 0,
            graph_expand_nodes: 0,
            hint_type: HintType::Semantic,
        };

        let mut got_alert = false;
        for _ in 0..15 {
            let alerts = agg.record_retrieval(&low_nrr_metrics);
            if alerts.iter().any(|a| a.alert_type == MetricsAlertType::LowNrr) {
                got_alert = true;
            }
        }
        assert!(got_alert, "Should trigger LowNrr alert after 10+ consecutive low NRR");
    }

    // =====================================================================
    // Test: MetricsAggregator — high abstention rate alert
    // =====================================================================

    #[test]
    fn test_aggregator_high_abstention_alert() {
        let mut agg = MetricsAggregator::with_defaults(1.0);

        let abstention_metrics = OnlineRetrievalMetrics {
            result_count: 0,
            avg_score: 0.0,
            max_score: 0.0,
            abstention_triggered: true,
            retrieval_level: 3,
            graph_expand_nodes: 0,
            hint_type: HintType::Semantic,
        };

        let mut got_alert = false;
        for _ in 0..40 {
            let alerts = agg.record_retrieval(&abstention_metrics);
            if alerts.iter().any(|a| a.alert_type == MetricsAlertType::HighAbstentionRate) {
                got_alert = true;
            }
        }
        assert!(got_alert, "Should trigger HighAbstentionRate alert");
    }

    // =====================================================================
    // Test: MetricsAggregator — conflict accuracy alert
    // =====================================================================

    #[test]
    fn test_aggregator_conflict_accuracy_alert() {
        let mut agg = MetricsAggregator::with_defaults(1.0);

        // Record 5 incorrect auto-resolutions (accuracy = 0%)
        for _ in 0..5 {
            let record = ConflictResolutionRecord {
                heuristic_type: "Evolution".to_string(),
                final_type: "Correction".to_string(),
                correct: false,
                auto_resolved: true,
            };
            agg.record_conflict(&record);
        }

        let alert = agg.record_conflict(&ConflictResolutionRecord {
            heuristic_type: "Evolution".to_string(),
            final_type: "Correction".to_string(),
            correct: false,
            auto_resolved: true,
        });

        assert!(
            alert.is_some(),
            "Should alert when conflict accuracy drops below 80%"
        );
        assert_eq!(alert.unwrap().alert_type, MetricsAlertType::LowConflictAccuracy);
    }

    // =====================================================================
    // Test: MetricsAggregator — high degradation rate alert
    // =====================================================================

    #[test]
    fn test_aggregator_degradation_alert() {
        let mut agg = MetricsAggregator::with_defaults(1.0);

        let degraded_metrics = OnlineRetrievalMetrics {
            result_count: 2,
            avg_score: 0.5,
            max_score: 0.7,
            abstention_triggered: false,
            retrieval_level: 2,
            graph_expand_nodes: 0,
            hint_type: HintType::FullText,
        };

        let mut got_alert = false;
        for _ in 0..15 {
            let alerts = agg.record_retrieval(&degraded_metrics);
            if alerts.iter().any(|a| a.alert_type == MetricsAlertType::HighDegradationRate) {
                got_alert = true;
            }
        }
        assert!(got_alert, "Should trigger HighDegradationRate alert");
    }

    // =====================================================================
    // Test: AlertThresholds defaults
    // =====================================================================

    #[test]
    fn test_alert_thresholds_defaults() {
        let t = AlertThresholds::default();
        assert!((t.nrr_warning - 0.5).abs() < f32::EPSILON);
        assert_eq!(t.nrr_consecutive_limit, 10);
        assert!((t.abstention_rate_high - 0.3).abs() < f32::EPSILON);
        assert!((t.abstention_rate_low - 0.05).abs() < f32::EPSILON);
        assert!((t.conflict_accuracy_min - 0.8).abs() < f32::EPSILON);
        assert!((t.degradation_rate_high - 0.2).abs() < f32::EPSILON);
    }
}
