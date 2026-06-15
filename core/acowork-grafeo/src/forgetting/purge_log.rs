//! Purge logging and recovery.
//!
//! When a memory node is purged, its full snapshot is retained in a special
//! `PurgeLog` node for 30 days, allowing accidental deletions to be recovered.

use chrono::{DateTime, Duration, Utc};
use grafeo_common::types::{NodeId, Value};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::labels;

/// Label used for purge-log entries inside Grafeo.
pub const PURGE_LOG_LABEL: &str = "PurgeLog";

/// Purge reason for audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PurgeReason {
    /// Dormant for too long (> retention_days) and low importance.
    TimeExpired {
        /// How many days the node was dormant before purge.
        dormant_days: u32,
        /// The node's importance score at purge time.
        importance: f32,
    },
    /// Capacity pressure triggered cleanup.
    CapacityPressure {
        /// Storage usage percentage that triggered the purge.
        usage_percent: f32,
        /// The node's decay score at purge time.
        decay_score: f32,
    },
    /// User manually requested deletion.
    UserManual,
}

impl PurgeReason {
    /// Serialize to a JSON string for storage.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Deserialize from a JSON string.
    pub fn from_json(s: &str) -> Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

/// A record of a purged node for potential recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct PurgeLogEntry {
    /// ID of the purged node.
    pub node_id: NodeId,
    /// Original label of the purged node.
    pub label: String,
    /// Serialized node properties.
    pub properties_json: String,
    /// Why the node was purged.
    pub purge_reason: PurgeReason,
    /// When the node was purged.
    pub purged_at: DateTime<Utc>,
    /// Until when the node can be recovered.
    pub recoverable_until: DateTime<Utc>,
}

impl PurgeLogEntry {
    /// Convert to Grafeo node properties for storage.
    pub fn to_properties(&self) -> Vec<(String, Value)> {
        let purged_ts =
            grafeo_common::types::Timestamp::from_micros(self.purged_at.timestamp_micros());
        let recover_ts =
            grafeo_common::types::Timestamp::from_micros(self.recoverable_until.timestamp_micros());

        vec![
            (
                "node_id".to_string(),
                Value::from(self.node_id.as_u64() as i64),
            ),
            ("label".to_string(), Value::from(self.label.as_str())),
            (
                "properties_json".to_string(),
                Value::from(self.properties_json.as_str()),
            ),
            (
                "purge_reason".to_string(),
                Value::from(self.purge_reason.to_json().as_str()),
            ),
            ("purged_at".to_string(), Value::from(purged_ts)),
            ("recoverable_until".to_string(), Value::from(recover_ts)),
        ]
    }

    /// Reconstruct from Grafeo node properties.
    pub fn from_properties(_id: NodeId, props: &[(String, Value)]) -> Option<Self> {
        let map: std::collections::HashMap<&str, &Value> =
            props.iter().map(|(k, v)| (k.as_str(), v)).collect();

        let node_id = map
            .get("node_id")
            .and_then(|v| v.as_int64())
            .map(|id| NodeId::new(id as u64))?;
        let label = map.get("label")?.as_str()?.to_string();
        let properties_json = map.get("properties_json")?.as_str()?.to_string();
        let purge_reason = PurgeReason::from_json(map.get("purge_reason")?.as_str()?).ok()?;
        let purged_at = map
            .get("purged_at")
            .and_then(|v| v.as_timestamp())
            .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()))?;
        let recoverable_until = map
            .get("recoverable_until")
            .and_then(|v| v.as_timestamp())
            .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()))?;

        Some(PurgeLogEntry {
            node_id,
            label,
            properties_json,
            purge_reason,
            purged_at,
            recoverable_until,
        })
    }
}

impl GrafeoStore {
    /// Purge a dormant node (Path 1: time expired).
    ///
    /// Dormant > retention_days AND importance < 0.5.
    /// Returns the purge-log entries for nodes that were actually purged.
    pub fn purge_expired_dormant(&self, retention_days: u32) -> Result<Vec<PurgeLogEntry>> {
        let mut purged = Vec::new();
        let now = Utc::now();

        for label in [labels::KNOWLEDGE, labels::PROCEDURAL] {
            let graph = self.db.graph_store();
            for node_id in graph.nodes_by_label(label) {
                if let Some(node) = self.db.get_node(node_id) {
                    // Only process Dormant nodes.
                    let status = node
                        .properties
                        .get(&"status".into())
                        .and_then(|v| v.as_str())
                        .unwrap_or("Active");
                    if status != "Dormant" {
                        continue;
                    }

                    let importance = node
                        .properties
                        .get(&"importance".into())
                        .and_then(|v| v.as_float64())
                        .unwrap_or(0.0) as f32;

                    if importance >= 0.5 {
                        continue;
                    }

                    let dormant_since = node
                        .properties
                        .get(&"dormant_since".into())
                        .and_then(|v| v.as_timestamp())
                        .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()));

                    if let Some(ds) = dormant_since {
                        let days_dormant = (now - ds).num_days() as u32;
                        if days_dormant > retention_days {
                            let entry = self.purge_node(
                                node_id,
                                label,
                                &node.properties,
                                PurgeReason::TimeExpired {
                                    dormant_days: days_dormant,
                                    importance,
                                },
                            )?;
                            purged.push(entry);
                        }
                    }
                }
            }
        }

        Ok(purged)
    }

    /// Purge under capacity pressure (Path 2).
    ///
    /// Purges the lowest decay_score Dormant nodes up to `target_count`.
    pub fn purge_by_capacity(&self, target_count: usize) -> Result<Vec<PurgeLogEntry>> {
        let mut candidates: Vec<(NodeId, &str, f32)> = Vec::new();

        for label in [labels::KNOWLEDGE, labels::PROCEDURAL] {
            let graph = self.db.graph_store();
            for node_id in graph.nodes_by_label(label) {
                if let Some(node) = self.db.get_node(node_id) {
                    let status = node
                        .properties
                        .get(&"status".into())
                        .and_then(|v| v.as_str())
                        .unwrap_or("Active");
                    if status != "Dormant" {
                        continue;
                    }

                    let decay_score = node
                        .properties
                        .get(&"decay_score".into())
                        .and_then(|v| v.as_float64())
                        .unwrap_or(1.0) as f32;

                    candidates.push((node_id, label, decay_score));
                }
            }
        }

        // Sort by decay_score ascending (lowest first).
        candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut purged = Vec::new();
        for (node_id, label, decay_score) in candidates.into_iter().take(target_count) {
            if let Some(node) = self.db.get_node(node_id) {
                let entry = self.purge_node(
                    node_id,
                    label,
                    &node.properties,
                    PurgeReason::CapacityPressure {
                        usage_percent: 0.0,
                        decay_score,
                    },
                )?;
                purged.push(entry);
            }
        }

        Ok(purged)
    }

    /// Purge by user request (Path 3).
    pub fn purge_by_user(&self, node_id: NodeId) -> Result<PurgeLogEntry> {
        let node = self.db.get_node(node_id).ok_or_else(|| {
            crate::error::GrafeoError::Memory(format!("node not found: {node_id}"))
        })?;

        let label = node
            .labels
            .first()
            .map(|l| l.as_str())
            .unwrap_or(labels::KNOWLEDGE);

        self.purge_node(node_id, label, &node.properties, PurgeReason::UserManual)
    }

    /// Recover a purged node from the purge log (within 30-day window).
    ///
    /// Returns the ID of the recreated node, or `None` if recovery failed.
    pub fn recover_purged_node(&self, node_id: NodeId) -> Result<Option<NodeId>> {
        let graph = self.db.graph_store();
        for log_id in graph.nodes_by_label(PURGE_LOG_LABEL) {
            if let Some(log_node) = self.db.get_node(log_id) {
                let stored_original_id = log_node
                    .properties
                    .get(&"node_id".into())
                    .and_then(|v| v.as_int64())
                    .map(|id| NodeId::new(id as u64));

                if stored_original_id != Some(node_id) {
                    continue;
                }

                // Check if still recoverable.
                let recoverable_until = log_node
                    .properties
                    .get(&"recoverable_until".into())
                    .and_then(|v| v.as_timestamp())
                    .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()));

                if let Some(deadline) = recoverable_until
                    && Utc::now() > deadline
                {
                    continue;
                }

                // Re-create the original node.
                let label = log_node
                    .properties
                    .get(&"label".into())
                    .and_then(|v| v.as_str())
                    .unwrap_or(labels::KNOWLEDGE)
                    .to_string();
                let properties_json = log_node
                    .properties
                    .get(&"properties_json".into())
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();

                let props: Vec<(String, Value)> =
                    serde_json::from_str(&properties_json).unwrap_or_default();
                let new_id =
                    self.store_node(&label, props.iter().map(|(k, v)| (k.as_str(), v.clone())))?;

                // Set status to Active.
                self.db
                    .set_node_property(new_id, "status", Value::from("Active"));

                // Remove the purge log entry.
                self.db.delete_node(log_id);

                return Ok(Some(new_id));
            }
        }

        Ok(None)
    }

    /// Clean up expired purge log entries (older than 30 days).
    ///
    /// Returns the number of entries removed.
    pub fn cleanup_purge_log(&self) -> Result<usize> {
        let mut removed = 0;
        let now = Utc::now();

        let graph = self.db.graph_store();
        for log_id in graph.nodes_by_label(PURGE_LOG_LABEL) {
            if let Some(log_node) = self.db.get_node(log_id) {
                let recoverable_until = log_node
                    .properties
                    .get(&"recoverable_until".into())
                    .and_then(|v| v.as_timestamp())
                    .and_then(|ts| DateTime::from_timestamp_micros(ts.as_micros()));

                if let Some(deadline) = recoverable_until
                    && now > deadline
                {
                    self.db.delete_node(log_id);
                    removed += 1;
                }
            }
        }

        Ok(removed)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Store a purge log entry as a special `PurgeLog` node.
    fn store_purge_log(&self, entry: &PurgeLogEntry) -> Result<NodeId> {
        let id = self.store_node(
            PURGE_LOG_LABEL,
            entry
                .to_properties()
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone())),
        )?;
        Ok(id)
    }

    /// Internal: purge a single node and create a purge log.
    fn purge_node(
        &self,
        node_id: NodeId,
        label: &str,
        properties: &grafeo_common::types::PropertyMap,
        reason: PurgeReason,
    ) -> Result<PurgeLogEntry> {
        let now = Utc::now();
        let recoverable_until = now + Duration::days(30);

        // Serialize properties to JSON.
        let props_vec: Vec<(String, Value)> = properties
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.clone()))
            .collect();
        let properties_json =
            serde_json::to_string(&props_vec).unwrap_or_else(|_| "{}".to_string());

        let entry = PurgeLogEntry {
            node_id,
            label: label.to_string(),
            properties_json,
            purge_reason: reason,
            purged_at: now,
            recoverable_until,
        };

        // Store purge log before deleting the node.
        self.store_purge_log(&entry)?;

        // Delete the original node.
        self.db.delete_node(node_id);

        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeStatus;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn create_dormant_node(
        store: &GrafeoStore,
        label: &str,
        importance: f64,
        dormant_days: i64,
    ) -> NodeId {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        let dormant_ts =
            grafeo_common::types::Timestamp::from_micros(now - (dormant_days * 86400 * 1_000_000));

        store
            .store_node(
                label,
                [
                    ("status", Value::from(NodeStatus::Dormant.as_str())),
                    ("importance", Value::from(importance)),
                    ("dormant_since", Value::from(dormant_ts)),
                ],
            )
            .unwrap()
    }

    #[test]
    fn test_purge_expired_dormant() {
        let store = test_store();

        // Dormant for 100 days, low importance -> should be purged.
        let old_low = create_dormant_node(&store, labels::KNOWLEDGE, 0.3, 100);
        // Dormant for 100 days, high importance -> should NOT be purged.
        let old_high = create_dormant_node(&store, labels::KNOWLEDGE, 0.8, 100);
        // Dormant for 10 days, low importance -> should NOT be purged.
        let recent_low = create_dormant_node(&store, labels::KNOWLEDGE, 0.3, 10);

        let purged = store.purge_expired_dormant(90).unwrap();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].node_id, old_low);

        // Verify the node is deleted.
        assert!(store.db.get_node(old_low).is_none());
        assert!(store.db.get_node(old_high).is_some());
        assert!(store.db.get_node(recent_low).is_some());
    }

    #[test]
    fn test_purge_by_capacity() {
        let store = test_store();

        let n1 = create_dormant_node(&store, labels::KNOWLEDGE, 0.2, 10);
        let n2 = create_dormant_node(&store, labels::KNOWLEDGE, 0.2, 10);
        let n3 = create_dormant_node(&store, labels::KNOWLEDGE, 0.2, 10);

        // Set decay_score so n1 < n2 < n3.
        store
            .db
            .set_node_property(n1, "decay_score", Value::from(0.1f64));
        store
            .db
            .set_node_property(n2, "decay_score", Value::from(0.2f64));
        store
            .db
            .set_node_property(n3, "decay_score", Value::from(0.3f64));

        let purged = store.purge_by_capacity(2).unwrap();
        assert_eq!(purged.len(), 2);

        // n1 and n2 should be purged (lowest scores).
        assert!(store.db.get_node(n1).is_none());
        assert!(store.db.get_node(n2).is_none());
        assert!(store.db.get_node(n3).is_some());
    }

    #[test]
    fn test_purge_by_user() {
        let store = test_store();
        let node_id = create_dormant_node(&store, labels::KNOWLEDGE, 0.5, 10);

        let entry = store.purge_by_user(node_id).unwrap();
        assert_eq!(entry.node_id, node_id);
        assert_eq!(entry.purge_reason, PurgeReason::UserManual);

        assert!(store.db.get_node(node_id).is_none());
    }

    #[test]
    fn test_recover_purged_node() {
        let store = test_store();
        let node_id = create_dormant_node(&store, labels::KNOWLEDGE, 0.5, 100);

        store.purge_by_user(node_id).unwrap();
        assert!(store.db.get_node(node_id).is_none());

        let recovered = store.recover_purged_node(node_id).unwrap();
        assert!(recovered.is_some());

        let new_id = recovered.unwrap();
        let node = store.db.get_node(new_id).unwrap();
        let status = node
            .properties
            .get(&"status".into())
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(status, "Active");
    }

    #[test]
    fn test_cleanup_purge_log() {
        let store = test_store();
        let node_id = create_dormant_node(&store, labels::KNOWLEDGE, 0.5, 100);

        store.purge_by_user(node_id).unwrap();

        // Manually set recoverable_until to the past.
        let graph = store.db.graph_store();
        let log_ids: Vec<NodeId> = graph.nodes_by_label(PURGE_LOG_LABEL);
        assert_eq!(log_ids.len(), 1);

        let past = grafeo_common::types::Timestamp::from_micros(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64
                - 31 * 86400 * 1_000_000,
        );
        store
            .db
            .set_node_property(log_ids[0], "recoverable_until", Value::from(past));

        let removed = store.cleanup_purge_log().unwrap();
        assert_eq!(removed, 1);
        assert!(store.db.get_node(log_ids[0]).is_none());
    }
}
