//! Purge log — audit archive for archived (forgotten) episodic nodes.
//!
//! When a Dormant episodic node is archived by the forgetting engine, its
//! full property snapshot is retained in a special `PurgeLog` node with a
//! 30-day `recoverable_until` marker (surfaced by memory statistics).

use chrono::{DateTime, Duration, Utc};
use grafeo_common::types::{NodeId, Value};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::grafeo::GrafeoStore;

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
    pub(crate) fn purge_node(
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
