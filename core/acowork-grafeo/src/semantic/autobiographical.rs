//! AutobiographicalNode storage.
//!
//! Autobiographical nodes never participate in decay - their status is
//! forced to [`NodeStatus::Active`] on every store operation (except
//! when explicitly set to Dormant for History compression).

use chrono::Utc;
use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{AutobioCategory, AutobiographicalNode, NodeStatus, labels};

impl GrafeoStore {
    /// Store an autobiographical node.
    ///
    /// The `status` field is overwritten to [`NodeStatus::Active`] regardless
    /// of the value supplied in `node`.
    pub fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<NodeId> {
        if let Some(id) = node.id {
            return self.update_autobiographical(node).map(|_| id);
        }

        let mut node = node.clone();
        node.status = NodeStatus::Active;

        let props = node.to_properties();
        let id = self.store_node(
            labels::AUTOBIOGRAPHICAL,
            props.iter().map(|(k, v)| (k.as_str(), v.clone())),
        )?;
        Ok(id)
    }

    /// Find all autobiographical nodes of a given category.
    pub fn find_autobiographical_by_category(
        &self,
        category: AutobioCategory,
    ) -> Result<Vec<AutobiographicalNode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::AUTOBIOGRAPHICAL);
        let cat_str = category.as_str();

        let mut results = Vec::new();
        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let matches = n
                    .get_property("category")
                    .and_then(Value::as_str)
                    .map(|c| c == cat_str)
                    .unwrap_or(false);

                if matches {
                    let props: Vec<(String, Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    results.push(AutobiographicalNode::from_properties(id, &props)?);
                }
            }
        }
        Ok(results)
    }

    /// Find an autobiographical node by its `key` (e.g. "name", "location").
    pub fn find_autobiographical_by_key(&self, key: &str) -> Result<Option<AutobiographicalNode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::AUTOBIOGRAPHICAL);

        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let matches = n
                    .get_property("key")
                    .and_then(Value::as_str)
                    .map(|k| k == key)
                    .unwrap_or(false);

                if matches {
                    let props: Vec<(String, Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    return Ok(Some(AutobiographicalNode::from_properties(id, &props)?));
                }
            }
        }
        Ok(None)
    }

    /// Update an existing autobiographical node.
    ///
    /// By default, autobiographical nodes are kept Active. However, the
    /// History compression flow (P2-3) may explicitly set status to
    /// Dormant. This method respects the status in the provided node
    /// **only if** it is explicitly set to Dormant; otherwise it forces
    /// Active (the safe default).
    pub fn update_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        let id = node.id.ok_or_else(|| {
            crate::error::GrafeoError::Memory(
                "cannot update autobiographical node without an ID".to_string(),
            )
        })?;

        let mut node = node.clone();
        // Respect explicit Dormant (used by History compression).
        // All other statuses ->? force Active (safety net).
        if node.status != NodeStatus::Dormant {
            node.status = NodeStatus::Active;
        }
        node.updated_at = Utc::now();

        let props = node.to_properties();
        self.update_node(id, props.iter().map(|(k, v)| (k.as_str(), v.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn make_node(category: AutobioCategory, key: &str, value: &str) -> AutobiographicalNode {
        AutobiographicalNode {
            id: None,
            category,
            key: key.to_string(),
            value: value.to_string(),
            confidence: 1.0,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Dormant, // intentionally wrong; store should force Active
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_store_autobiographical_forces_active() {
        let store = test_store();
        let node = make_node(AutobioCategory::Identity, "name", "WeatherBot");
        let id = store.store_autobiographical(&node).unwrap();

        let fetched = store.db.get_node(id).unwrap();
        let status = fetched
            .get_property("status")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(status, "Active");
    }

    #[test]
    fn test_find_autobiographical_by_category() {
        let store = test_store();
        store
            .store_autobiographical(&make_node(AutobioCategory::Identity, "name", "Bot"))
            .unwrap();
        store
            .store_autobiographical(&make_node(AutobioCategory::Capability, "skill", "weather"))
            .unwrap();
        store
            .store_autobiographical(&make_node(AutobioCategory::Identity, "version", "1.0"))
            .unwrap();

        let identity = store
            .find_autobiographical_by_category(AutobioCategory::Identity)
            .unwrap();
        assert_eq!(identity.len(), 2);

        let capability = store
            .find_autobiographical_by_category(AutobioCategory::Capability)
            .unwrap();
        assert_eq!(capability.len(), 1);
        assert_eq!(capability[0].key, "skill");
    }

    #[test]
    fn test_find_autobiographical_by_key() {
        let store = test_store();
        store
            .store_autobiographical(&make_node(AutobioCategory::Limitation, "max_days", "7"))
            .unwrap();

        let found = store.find_autobiographical_by_key("max_days").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().value, "7");

        let missing = store.find_autobiographical_by_key("min_days").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_update_autobiographical() {
        let store = test_store();
        let node = make_node(AutobioCategory::History, "milestone", "first_release");
        let id = store.store_autobiographical(&node).unwrap();

        let updated = AutobiographicalNode {
            id: Some(id),
            status: NodeStatus::Active, // Explicitly set Active
            value: "second_release".to_string(),
            confidence: 0.95,
            ..node
        };
        store.update_autobiographical(&updated).unwrap();

        let fetched = store.db.get_node(id).unwrap();
        let value = fetched
            .get_property("value")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(value, "second_release");

        let status = fetched
            .get_property("status")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(status, "Active");
    }

    #[test]
    fn test_update_autobiographical_respects_dormant() {
        // P2-3: History compression marks old History nodes as Dormant.
        // This test verifies that update_autobiographical respects
        // explicitly-set Dormant status.
        let store = test_store();
        let node = make_node(AutobioCategory::History, "old_milestone", "ancient_event");
        let id = store.store_autobiographical(&node).unwrap();

        // Mark as Dormant (simulating History compression).
        let dormant = AutobiographicalNode {
            id: Some(id),
            status: NodeStatus::Dormant,
            ..node
        };
        store.update_autobiographical(&dormant).unwrap();

        let fetched = store.db.get_node(id).unwrap();
        let status = fetched
            .get_property("status")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(
            status, "Dormant",
            "Dormant status should be respected for History compression"
        );
    }
}
