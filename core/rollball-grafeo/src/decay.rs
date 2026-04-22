//! Memory decay mechanism.

use std::time::{SystemTime, UNIX_EPOCH};

use grafeo_common::types::NodeId;
use grafeo_engine::cdc::ChangeEvent;

use crate::error::Result;
use crate::grafeo::GrafeoStore;

/// Configuration for the memory decay algorithm.
#[derive(Debug, Clone, Copy)]
pub struct DecayConfig {
    /// Decay rate constant (default: 0.03).
    pub lambda: f64,
    /// Minimum activity floor (default: 0.05).
    pub floor: f64,
    /// Increment per access event (default: 0.1).
    pub access_per_hit: f64,
    /// Maximum boost from access history (default: 0.5).
    pub boost_cap: f64,
    /// Threshold below which a node is considered dormant (default: 0.3).
    pub dormant_threshold: f64,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            lambda: 0.03,
            floor: 0.05,
            access_per_hit: 0.1,
            boost_cap: 0.5,
            dormant_threshold: 0.3,
        }
    }
}

impl GrafeoStore {
    /// Return the full CDC change history for a node.
    ///
    /// Events are ordered chronologically by epoch.
    pub fn node_history(&self, node_id: NodeId) -> Result<Vec<ChangeEvent>> {
        let history = self.db.history(node_id)?;
        Ok(history)
    }

    /// Compute a decay score for a node based on its change history.
    ///
    /// The score starts at 1.0 and decays exponentially with the time since
    /// the most recent change event. Access events (updates) provide a small
    /// boost capped at `config.boost_cap`.
    pub fn compute_decay_score(&self, node_id: NodeId, config: &DecayConfig) -> Result<f64> {
        let history = self.db.history(node_id)?;

        if history.is_empty() {
            return Ok(config.floor);
        }

        // Count update events as "accesses".
        let access_count = history
            .iter()
            .filter(|e| matches!(e.kind, grafeo_engine::cdc::ChangeKind::Update))
            .count() as f64;

        let access_boost = (access_count * config.access_per_hit).min(config.boost_cap);

        // Most recent event timestamp (physical millis in upper 48 bits of HLC).
        let last_event = history.last().unwrap();
        let last_ts_ms = last_event.timestamp.physical_ms();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let days_since = if now_ms > last_ts_ms {
            (now_ms - last_ts_ms) as f64 / (86_400_000.0)
        } else {
            0.0
        };

        // Exponential decay: score = e^(-lambda * days).
        let decayed = (-config.lambda * days_since).exp();
        let score = (decayed + access_boost).max(config.floor).min(1.0);

        Ok(score)
    }

    /// Scan all memory nodes and return those whose decay score is below the
    /// given threshold.
    ///
    /// Nodes across all memory labels are evaluated.
    pub fn scan_dormant_nodes(&self, threshold: f64) -> Result<Vec<NodeId>> {
        let config = DecayConfig::default();
        let graph = self.db.graph_store();
        let mut dormant = Vec::new();

        for label in crate::grafeo::MEMORY_LABELS {
            for node_id in graph.nodes_by_label(label) {
                let score = self.compute_decay_score(node_id, &config)?;
                if score < threshold {
                    dormant.push(node_id);
                }
            }
        }

        Ok(dormant)
    }
}
