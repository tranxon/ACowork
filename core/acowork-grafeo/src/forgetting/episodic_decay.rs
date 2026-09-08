//! Episodic forgetting — pure time decay.
//!
//! Episodic nodes are real event records with retrieval value, so
//! forgetting is **gradual and time-only**: no consolidated/importance
//! branches, no age cliffs.
//!
//! The retention curve is a half-life exponential decay:
//!
//! ```text
//! retention = exp(-ln2 * age_days / half_life_days)
//! ```
//!
//! - Active nodes stay fully retrievable while `retention >= dormant_threshold`;
//!   retrieval down-weights by `retention` (progressive decay).
//! - Active → Dormant once `retention < dormant_threshold`.
//! - Dormant → archived to the PurgeLog (30-day recovery window) after
//!   `archive_days` of dormancy.
//!
//! Forgetting is opt-in: `EpisodicDecayConfig::enabled = false` (default)
//! makes the scan a no-op. Only `Episodic` nodes are touched — the
//! sediment layer (Knowledge / Procedural / Autobiographical) is
//! intentionally excluded and keeps the multiplicative `run_decay_scan`
//! (retained for future use).

use chrono::{DateTime, Utc};
use grafeo_core::graph::lpg::Node;

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{NodeStatus, labels};

use acowork_memory::{DecayScanResult, EpisodicDecayConfig};

use super::purge_log::PurgeReason;

impl GrafeoStore {
    /// Run episodic forgetting: pure time decay scan over `Episodic` nodes.
    ///
    /// Returns a [`DecayScanResult`] with `to_dormant` (Active → Dormant)
    /// and `purged` (Dormant → PurgeLog archive) counts. When
    /// `config.enabled` is false, returns zeroes without touching the store.
    pub fn run_episodic_decay_scan(
        &self,
        config: &EpisodicDecayConfig,
    ) -> Result<DecayScanResult> {
        let mut result = DecayScanResult::default();
        if !config.enabled {
            return Ok(result);
        }

        let now = Utc::now();
        let graph = self.db.graph_store();

        for node_id in graph.nodes_by_label(labels::EPISODIC) {
            let Some(node) = self.db.get_node(node_id) else { continue };

            let status = node
                .properties
                .get(&"status".into())
                .and_then(|v| v.as_str())
                .unwrap_or(NodeStatus::Active.as_str());

            // 1. Active → Dormant when retention < dormant_threshold.
            if status == NodeStatus::Active.as_str() {
                let age_days = age_days_from_props(&node, now);
                let retention = config.retention(age_days);
                if retention < f64::from(config.dormant_threshold) {
                    self.transition_to_dormant(node_id)?;
                    result.to_dormant += 1;
                }
                continue;
            }

            // 2. Dormant → archived to PurgeLog after archive_days.
            if status == NodeStatus::Dormant.as_str() {
                let dormant_days = node
                    .properties
                    .get(&"dormant_since".into())
                    .and_then(|v| v.as_timestamp())
                    .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()))
                    .map(|ds| (now - ds).num_days())
                    .unwrap_or(0);
                if dormant_days >= config.archive_days as i64 {
                    self.purge_node(
                        node_id,
                        labels::EPISODIC,
                        &node.properties,
                        PurgeReason::TimeExpired {
                            dormant_days: dormant_days as u32,
                            importance: 0.0,
                        },
                    )?;
                    result.purged += 1;
                }
            }
        }

        Ok(result)
    }
}

/// Age of a node in days, from `created_at` (0.0 when missing).
fn age_days_from_props(node: &Node, now: DateTime<Utc>) -> f64 {
    node.properties
        .get(&"created_at".into())
        .and_then(|v| v.as_timestamp())
        .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()))
        .map(|dt| (now - dt).num_seconds() as f64 / 86400.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grafeo_common::types::{NodeId, Value};
    use crate::types::NodeStatus;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn create_episodic(store: &GrafeoStore, age_days: f64) -> NodeId {
        let created = Utc::now() - chrono::Duration::days(age_days as i64);
        let id = store
            .store_node(
                labels::EPISODIC,
                [
                    ("content", Value::from("event record")),
                    ("status", Value::from(NodeStatus::Active.as_str())),
                ],
            )
            .unwrap();
        store.db().set_node_property(
            id,
            "created_at",
            Value::from(grafeo_common::types::Timestamp::from_micros(
                created.timestamp_micros(),
            )),
        );
        id
    }

    #[test]
    fn disabled_config_is_noop() {
        let store = test_store();
        create_episodic(&store, 1000.0);
        let cfg = EpisodicDecayConfig {
            enabled: false,
            ..Default::default()
        };
        let result = store.run_episodic_decay_scan(&cfg).unwrap();
        assert_eq!(result.to_dormant, 0);
        assert_eq!(result.purged, 0);
    }

    #[test]
    fn young_nodes_stay_active() {
        let store = test_store();
        create_episodic(&store, 10.0);
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            dormant_threshold: 0.1,
            archive_days: 90,
        };
        let result = store.run_episodic_decay_scan(&cfg).unwrap();
        assert_eq!(result.to_dormant, 0);
    }

    #[test]
    fn very_old_nodes_become_dormant() {
        let store = test_store();
        let id = create_episodic(&store, 1800.0); // 5 years, retention ≈ 0
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            dormant_threshold: 0.1,
            archive_days: 90,
        };
        let result = store.run_episodic_decay_scan(&cfg).unwrap();
        assert_eq!(result.to_dormant, 1);
        let node = store.db().get_node(id).unwrap();
        assert_eq!(
            node.properties.get(&"status".into()).unwrap().as_str(),
            Some(NodeStatus::Dormant.as_str())
        );
    }

    #[test]
    fn dormant_nodes_archived_after_deadline() {
        let store = test_store();
        let id = create_episodic(&store, 1800.0);
        // Transition to Dormant with an old dormant_since.
        store.transition_to_dormant(id).unwrap();
        let ds = Utc::now() - chrono::Duration::days(200);
        store.db().set_node_property(
            id,
            "dormant_since",
            Value::from(grafeo_common::types::Timestamp::from_micros(
                ds.timestamp_micros(),
            )),
        );
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            dormant_threshold: 0.1,
            archive_days: 90,
        };
        let result = store.run_episodic_decay_scan(&cfg).unwrap();
        assert_eq!(result.to_dormant, 0, "already dormant");
        assert_eq!(result.purged, 1, "old dormant node archived");
    }

    #[test]
    fn retention_formula_half_life() {
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            dormant_threshold: 0.1,
            archive_days: 90,
        };
        // At exactly the half-life, retention should be ≈ 0.5.
        let r = cfg.retention(180.0);
        assert!((r - 0.5).abs() < 1e-6, "retention at half-life = {r}");
        // At 2× half-life, retention ≈ 0.25.
        let r2 = cfg.retention(360.0);
        assert!((r2 - 0.25).abs() < 1e-6, "retention at 2× half-life = {r2}");
        // Zero half-life disables decay.
        let zero = EpisodicDecayConfig {
            half_life_days: 0,
            ..cfg.clone()
        };
        assert_eq!(zero.retention(100000.0), 1.0);
    }
}

