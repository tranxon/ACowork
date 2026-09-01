//! Runtime statistics and SLA monitoring for the Grafeo memory system.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::forgetting::decay::{compute_decay_score, DecayConfig};
use crate::grafeo::GrafeoStore;
use crate::types::NodeStatus;

/// Snapshot of memory system runtime statistics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryStats {
    /// Node count per label (e.g., Episodic, Knowledge, PurgeLog).
    pub label_counts: HashMap<String, usize>,
    /// Total number of retrieval operations performed.
    pub total_queries: u64,
    /// Average retrieval latency in milliseconds (placeholder for Phase 2).
    pub avg_latency_ms: f32,
    /// Total number of conflicts detected since startup.
    pub conflict_total: u64,
    /// Conflict count broken down by type string.
    pub conflict_by_type: HashMap<String, u64>,
    /// Number of nodes currently in Dormant status.
    pub dormant_count: usize,
    /// Number of purged nodes (PurgeLog entries).
    pub purged_count: usize,
}

/// Collect a statistics snapshot from a live GrafeoStore.
///
/// Uses `db.schema()` for label counts and a lightweight GQL query
/// to count dormant nodes. Purged nodes are inferred from the `PurgeLog`
/// label count.
pub fn collect_stats(store: &GrafeoStore) -> Result<MemoryStats> {
    let db = store.db();
    let schema = db.schema();

    let mut label_counts = HashMap::new();
    let mut purged_count = 0;

    if let grafeo_engine::admin::SchemaInfo::Lpg(lpg) = schema {
        for info in lpg.labels {
            if info.name == crate::forgetting::PURGE_LOG_LABEL {
                purged_count = info.count;
            }
            label_counts.insert(info.name, info.count);
        }
    }

    let dormant_count = count_dormant_nodes(db).unwrap_or(0);

    Ok(MemoryStats {
        label_counts,
        total_queries: 0,
        avg_latency_ms: 0.0,
        conflict_total: 0,
        conflict_by_type: HashMap::new(),
        dormant_count,
        purged_count,
    })
}

fn count_dormant_nodes(db: &grafeo_engine::GrafeoDB) -> Result<usize> {
    let result = db.execute("MATCH (n) RETURN n.status")?;
    let count = result
        .rows()
        .iter()
        .filter(|row| row.first().and_then(|v| v.as_str()) == Some("Dormant"))
        .count();
    Ok(count)
}

// ---------------------------------------------------------------------------
// Per-node status aggregation (for the memory panel stats API)
// ---------------------------------------------------------------------------

/// Aggregated node-level information consumed by the memory panel.
///
/// The decay score is computed using the exact same formula as
/// `forgetting::scan` so that the dashboard always reflects the
/// numbers the forgetting engine actually acts on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeStatusAggregate {
    /// Count of nodes keyed by status string (`"Active"`, `"Dormant"`,
    /// `"Pending"` – see [`NodeStatus::as_str`]).
    pub by_status: HashMap<String, u64>,
    /// Total number of nodes examined across all requested labels.
    pub total_nodes: u64,
    /// Average decay score (0.0 ..= 1.0).  Falls back to `1.0` when no
    /// node has enough property data for the computation.
    pub avg_decay_score: f32,
}

/// Walk every node belonging to `labels` and collect per-status counts
/// together with an average decay score.
///
/// The caller decides which labels to include; typically this should be
/// the full set of memory-node labels (`Episodic`, `Knowledge`,
/// `Procedural`, `Autobiographical`) so that all user-visible nodes
/// contribute to the counts.
///
/// Nodes whose `status` property is missing default to
/// [`NodeStatus::Active`].
pub fn aggregate_node_status(
    store: &GrafeoStore,
    labels: &[&str],
) -> Result<NodeStatusAggregate> {
    let db = store.db();
    let graph = db.graph_store();
    let now = Utc::now();
    let decay_config = DecayConfig::default();

    let mut by_status: HashMap<String, u64> = HashMap::new();
    let mut total_nodes: u64 = 0;
    let mut decay_sum: f32 = 0.0;
    let mut decay_count: u64 = 0;

    for &label in labels {
        let node_ids = graph.nodes_by_label(label);
        for &nid in &node_ids {
            let Some(node) = graph.get_node(nid) else { continue };
            total_nodes += 1;

            // --- status (default: Active) ---
            let status = node
                .properties
                .get(&"status".into())
                .and_then(|v| v.as_str())
                .unwrap_or(NodeStatus::Active.as_str())
                .to_string();
            *by_status.entry(status).or_insert(0) += 1;

            // --- decay score (mirrors forgetting::scan) ---
            let importance = node
                .properties
                .get(&"importance".into())
                .and_then(|v| v.as_float64())
                .unwrap_or(0.5) as f32;

            let access_count = node
                .properties
                .get(&"access_count".into())
                .and_then(|v| v.as_int64())
                .unwrap_or(0) as u32;

            let days_since = node
                .properties
                .get(&"last_accessed".into())
                .and_then(|v| v.as_timestamp())
                .and_then(|ts| {
                    DateTime::from_timestamp_micros(ts.as_micros())
                        .map(|dt| (now - dt).num_seconds() as f64 / 86400.0)
                })
                .unwrap_or_else(|| {
                    // Fall back to created_at.
                    node.properties
                        .get(&"created_at".into())
                        .and_then(|v| v.as_timestamp())
                        .and_then(|ts| {
                            DateTime::from_timestamp_micros(ts.as_micros())
                                .map(|dt| (now - dt).num_seconds() as f64 / 86400.0)
                        })
                        .unwrap_or(0.0)
                });

            let score = compute_decay_score(
                &decay_config,
                importance,
                days_since.clamp(0.0, f64::MAX),
                access_count,
            );

            decay_sum += score;
            decay_count += 1;
        }
    }

    let avg_decay_score = if decay_count > 0 {
        decay_sum / decay_count as f32
    } else {
        1.0
    };

    Ok(NodeStatusAggregate {
        by_status,
        total_nodes,
        avg_decay_score,
    })
}

// ---------------------------------------------------------------------------
// SLA
// ---------------------------------------------------------------------------

/// SLA thresholds for hybrid search latency.
#[derive(Debug, Clone, PartialEq)]
pub struct SlaConfig {
    /// P99 latency threshold for 1K nodes (milliseconds).
    pub p99_1k_ms: f64,
    /// P99 latency threshold for 10K nodes (milliseconds).
    pub p99_10k_ms: f64,
}

impl Default for SlaConfig {
    fn default() -> Self {
        Self {
            p99_1k_ms: 100.0,
            p99_10k_ms: 500.0,
        }
    }
}

/// Current SLA compliance status.
#[derive(Debug, Clone, PartialEq)]
pub struct SlaStatus {
    /// Whether the 1K-node P99 target is currently met.
    pub p99_1k_met: bool,
    /// Whether the 10K-node P99 target is currently met.
    pub p99_10k_met: bool,
    /// Measured P99 latency in milliseconds (0.0 if unknown).
    pub measured_p99_ms: f64,
}

impl Default for SlaStatus {
    fn default() -> Self {
        Self {
            p99_1k_met: false,
            p99_10k_met: false,
            measured_p99_ms: 0.0,
        }
    }
}

/// Check SLA compliance against measured latency.
///
/// # Arguments
/// * `config` — SLA thresholds.
/// * `measured_p99_ms` — Measured P99 latency in milliseconds.
/// * `node_count` — Current approximate node count (for tier selection).
pub fn check_sla(config: &SlaConfig, measured_p99_ms: f64, node_count: usize) -> SlaStatus {
    let p99_1k_met = node_count < 1_000 && measured_p99_ms <= config.p99_1k_ms;
    let p99_10k_met = measured_p99_ms <= config.p99_10k_ms;

    SlaStatus {
        p99_1k_met,
        p99_10k_met,
        measured_p99_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    #[test]
    fn test_collect_stats_empty_store() {
        let store = test_store();
        let stats = collect_stats(&store).unwrap();
        assert_eq!(stats.dormant_count, 0);
        assert_eq!(stats.purged_count, 0);
    }

    #[test]
    fn test_sla_config_default() {
        let config = SlaConfig::default();
        assert!((config.p99_1k_ms - 100.0).abs() < f64::EPSILON);
        assert!((config.p99_10k_ms - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_check_sla_1k_met() {
        let config = SlaConfig::default();
        let status = check_sla(&config, 80.0, 500);
        assert!(status.p99_1k_met);
        assert!(status.p99_10k_met);
    }

    #[test]
    fn test_check_sla_1k_violated() {
        let config = SlaConfig::default();
        let status = check_sla(&config, 150.0, 500);
        assert!(!status.p99_1k_met);
        assert!(status.p99_10k_met);
    }

    #[test]
    fn test_check_sla_10k_violated() {
        let config = SlaConfig::default();
        let status = check_sla(&config, 600.0, 5_000);
        assert!(!status.p99_1k_met);
        assert!(!status.p99_10k_met);
    }

    #[test]
    fn test_sla_status_default() {
        let status = SlaStatus::default();
        assert!(!status.p99_1k_met);
        assert!(!status.p99_10k_met);
        assert!((status.measured_p99_ms).abs() < f64::EPSILON);
    }

    #[test]
    fn test_memory_stats_default() {
        let stats = MemoryStats::default();
        assert!(stats.label_counts.is_empty());
        assert_eq!(stats.total_queries, 0);
        assert_eq!(stats.conflict_total, 0);
        assert_eq!(stats.dormant_count, 0);
        assert_eq!(stats.purged_count, 0);
    }
}
