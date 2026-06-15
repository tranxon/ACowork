//! ProceduralNode storage.

use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{ProceduralNode, labels};

impl GrafeoStore {
    /// Store a procedural memory node.
    pub fn store_procedural(&self, node: &ProceduralNode) -> Result<NodeId> {
        if let Some(id) = node.id {
            return self.update_procedural(node).map(|_| id);
        }

        let props = node.to_properties();
        let id = self.store_node(
            labels::PROCEDURAL,
            props.iter().map(|(k, v)| (k.as_str(), v.clone())),
        )?;
        Ok(id)
    }

    /// Find procedural nodes whose `trigger_condition` contains the given keyword.
    pub fn find_procedural_by_trigger(
        &self,
        trigger: &str,
        limit: usize,
    ) -> Result<Vec<ProceduralNode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::PROCEDURAL);
        let trigger_lower = trigger.to_lowercase();

        let mut results = Vec::new();
        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let matches = n
                    .get_property("trigger_condition")
                    .and_then(Value::as_str)
                    .map(|t| t.to_lowercase().contains(&trigger_lower))
                    .unwrap_or(false);

                if matches {
                    let props: Vec<(String, Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    results.push(ProceduralNode::from_properties(id, &props)?);
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    /// Get a procedural node by ID.
    ///
    /// Returns `None` if the node doesn't exist or is not a Procedural node.
    pub fn get_procedural(&self, id: NodeId) -> Result<Option<ProceduralNode>> {
        let node = match self.db.get_node(id) {
            Some(n) => n,
            None => return Ok(None),
        };

        // Verify it's actually a Procedural node.
        let graph = self.db.graph_store();
        if !graph.nodes_by_label(labels::PROCEDURAL).contains(&id) {
            return Ok(None);
        }

        let props: Vec<(String, Value)> = node
            .properties_as_btree()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        Ok(Some(ProceduralNode::from_properties(id, &props)?))
    }

    /// Update an existing procedural node (e.g. increment success/fail counts).
    pub fn update_procedural(&self, node: &ProceduralNode) -> Result<()> {
        let id = node.id.ok_or_else(|| {
            crate::error::GrafeoError::Memory(
                "cannot update procedural node without an ID".to_string(),
            )
        })?;

        let props = node.to_properties();
        self.update_node(id, props.iter().map(|(k, v)| (k.as_str(), v.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeStatus;

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_dt() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn test_store_procedural() {
        let store = test_store();
        let node = ProceduralNode {
            id: None,
            name: "concise_output".to_string(),
            trigger_condition: "user asks for summary".to_string(),
            action_pattern: "reply in 3 sentences max".to_string(),
            success_count: 5,
            fail_count: 1,
            confidence: 0.85,
            activation_count: 0,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };

        let id = store.store_procedural(&node).unwrap();
        let graph = store.db.graph_store();
        assert!(graph.nodes_by_label(labels::PROCEDURAL).contains(&id));
    }

    #[test]
    fn test_find_procedural_by_trigger() {
        let store = test_store();
        let node = ProceduralNode {
            id: None,
            name: "format_table".to_string(),
            trigger_condition: "user requests a data table".to_string(),
            action_pattern: "output markdown table".to_string(),
            success_count: 10,
            fail_count: 0,
            confidence: 0.9,
            activation_count: 3,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap();

        let found = store.find_procedural_by_trigger("data table", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "format_table");

        let empty = store.find_procedural_by_trigger("chart", 5).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_update_procedural() {
        let store = test_store();
        let node = ProceduralNode {
            id: None,
            name: "retry".to_string(),
            trigger_condition: "network error".to_string(),
            action_pattern: "retry once".to_string(),
            success_count: 3,
            fail_count: 2,
            confidence: 0.7,
            activation_count: 0,
            source_skill: None,
            learned_from: "execution_failure".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        let id = store.store_procedural(&node).unwrap();

        let updated = ProceduralNode {
            id: Some(id),
            success_count: 4,
            confidence: 0.75,
            ..node
        };
        store.update_procedural(&updated).unwrap();

        let n = store.db.get_node(id).unwrap();
        let success = n
            .get_property("success_count")
            .and_then(Value::as_int64)
            .unwrap();
        assert_eq!(success, 4);
    }
}
