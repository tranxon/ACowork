//! Ambiguous conflict tracking and user confirmation flow.
//!
//! When the heuristic conflict detector cannot classify a conflict as
//! Evolution or Correction, both nodes are kept Active and marked with a
//! `conflict_group_id`. This module tracks those pending ambiguous conflicts
//! and provides the user-confirmation pipeline.

use chrono::{DateTime, Utc};
use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{labels, KnowledgeNode, NodeStatus};

/// An ambiguous conflict waiting for user confirmation.
#[derive(Debug, Clone)]
pub struct AmbiguousConflict {
    /// Unique group identifier shared by the conflicting nodes.
    pub conflict_group_id: String,
    /// First conflicting node.
    pub node_a_id: NodeId,
    /// Second conflicting node.
    pub node_b_id: NodeId,
    /// When the conflict was first detected.
    pub created_at: DateTime<Utc>,
}

impl GrafeoStore {
    /// Get all pending ambiguous conflicts.
    ///
    /// Scans all Knowledge nodes and groups those that share a
    /// `conflict_group_id` in their metadata.
    pub fn get_pending_ambiguous_conflicts(&self) -> Result<Vec<AmbiguousConflict>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        let mut groups: std::collections::HashMap<String, Vec<(NodeId, DateTime<Utc>)>> =
            std::collections::HashMap::new();

        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let props: Vec<(String, Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();

                if let Ok(kn) = KnowledgeNode::from_properties(id, &props)
                    && let Some(group_id) =
                        kn.metadata.get("conflict_group_id").and_then(|v| v.as_str())
                {
                    groups
                        .entry(group_id.to_string())
                        .or_default()
                        .push((id, kn.created_at));
                }
            }
        }

        let mut conflicts = Vec::new();
        for (group_id, mut nodes) in groups {
            if nodes.len() >= 2 {
                nodes.sort_by_key(|n| n.1);
                conflicts.push(AmbiguousConflict {
                    conflict_group_id: group_id,
                    node_a_id: nodes[0].0,
                    node_b_id: nodes[1].0,
                    created_at: nodes[0].1,
                });
            }
        }

        Ok(conflicts)
    }

    /// Get the count of pending ambiguous conflicts.
    pub fn count_pending_ambiguous(&self) -> Result<usize> {
        let conflicts = self.get_pending_ambiguous_conflicts()?;
        Ok(conflicts.len())
    }

    /// Check if user confirmation should be triggered (>= 3 pending).
    pub fn should_trigger_confirmation(&self) -> Result<bool> {
        Ok(self.count_pending_ambiguous()? >= 3)
    }

    /// Generate a hint for the LLM to naturally ask the user about ambiguous conflicts.
    ///
    /// Returns `None` when there are no pending ambiguous conflicts.
    pub fn generate_confirmation_hint(&self) -> Result<Option<String>> {
        let conflicts = self.get_pending_ambiguous_conflicts()?;
        if conflicts.is_empty() {
            return Ok(None);
        }

        let mut lines = Vec::new();
        for c in &conflicts {
            let node_a = self.get_knowledge(c.node_a_id)?;
            let node_b = self.get_knowledge(c.node_b_id)?;
            if let (Some(a), Some(b)) = (node_a, node_b) {
                lines.push(format!("- \"{}\" vs \"{}\"", a.object, b.object));
            }
        }

        if lines.is_empty() {
            return Ok(None);
        }

        let hint = format!(
            "There are {} ambiguous memory conflicts that need your confirmation:\\n{}",
            conflicts.len(),
            lines.join("\n")
        );

        Ok(Some(hint))
    }

    /// Resolve an ambiguous conflict with user's choice.
    ///
    /// Marks the chosen node as `Active` and demotes the other to `Dormant`,
    /// clearing the `conflict_group_id` metadata from both.
    pub fn resolve_ambiguous(
        &self,
        conflict_group_id: &str,
        keep_node_id: NodeId,
    ) -> Result<()> {
        let conflicts = self.get_pending_ambiguous_conflicts()?;
        let conflict = conflicts
            .into_iter()
            .find(|c| c.conflict_group_id == conflict_group_id)
            .ok_or_else(|| {
                crate::error::GrafeoError::Memory(format!(
                    "ambiguous conflict group {} not found",
                    conflict_group_id
                ))
            })?;

        let other_id = if keep_node_id == conflict.node_a_id {
            conflict.node_b_id
        } else if keep_node_id == conflict.node_b_id {
            conflict.node_a_id
        } else {
            return Err(crate::error::GrafeoError::Memory(
                "keep_node_id does not belong to this conflict group".to_string(),
            ));
        };

        // Mark the kept node as Active and clear conflict_group_id.
        if let Some(mut keep) = self.get_knowledge(keep_node_id)? {
            keep.status = NodeStatus::Active;
            keep.metadata.remove("conflict_group_id");
            keep.updated_at = Utc::now();
            self.update_knowledge(&keep)?;
        }

        // Mark the other node as Dormant and clear conflict_group_id.
        if let Some(mut other) = self.get_knowledge(other_id)? {
            other.status = NodeStatus::Dormant;
            other.metadata.remove("conflict_group_id");
            other.updated_at = Utc::now();
            self.update_knowledge(&other)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KnowledgeNode, KnowledgeSubType, DEFAULT_EMBEDDING_DIM};

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn const_emb(v: f32) -> Vec<f32> {
        vec![v; DEFAULT_EMBEDDING_DIM]
    }

    fn flipped_emb(flip_count: usize) -> Vec<f32> {
        let mut v = vec![1.0f32; DEFAULT_EMBEDDING_DIM];
        for i in 0..flip_count {
            v[DEFAULT_EMBEDDING_DIM - 1 - i] = -1.0;
        }
        v
    }

    // =====================================================================
    // Test 1: get_pending_ambiguous_conflicts returns tracked conflicts
    // =====================================================================

    #[test]
    fn test_get_pending_ambiguous_conflicts() {
        let store = test_store();

        // Create two conflicting nodes marked as ambiguous.
        let two_days_ago = Utc::now() - chrono::TimeDelta::days(2);
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "dark mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: two_days_ago,
            updated_at: two_days_ago,
            metadata: std::collections::HashMap::new(),
        };
        let existing_id = store.store_knowledge(&existing).unwrap();

        let input = crate::consolidation::MemoryStoreInput {
            content: "User prefers light mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: Some("light mode".to_string()),
            confidence: Some(0.88),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };
        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());

        let pending = store.get_pending_ambiguous_conflicts().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_a_id, existing_id);
    }

    // =====================================================================
    // Test 2: count_pending_ambiguous
    // =====================================================================

    #[test]
    fn test_count_pending_ambiguous() {
        let store = test_store();
        assert_eq!(store.count_pending_ambiguous().unwrap(), 0);

        // Trigger ambiguous conflict.
        let two_days_ago = Utc::now() - chrono::TimeDelta::days(2);
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "dark mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: two_days_ago,
            updated_at: two_days_ago,
            metadata: std::collections::HashMap::new(),
        };
        store.store_knowledge(&existing).unwrap();

        let input = crate::consolidation::MemoryStoreInput {
            content: "User prefers light mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: Some("light mode".to_string()),
            confidence: Some(0.88),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };
        store.process_memory_store(&input).unwrap();

        assert_eq!(store.count_pending_ambiguous().unwrap(), 1);
    }

    // =====================================================================
    // Test 3: should_trigger_confirmation true when >= 3 pending
    // =====================================================================

    #[test]
    fn test_should_trigger_confirmation() {
        let store = test_store();
        assert!(!store.should_trigger_confirmation().unwrap());

        // Create 3 ambiguous conflicts by injecting nodes with conflict_group_id.
        for i in 0..3 {
            let group_id = format!("cg_test_{}", i);
            let node_a = KnowledgeNode {
                id: None,
                subject: "user".to_string(),
                predicate: format!("prefers_{}", i),
                object: "A".to_string(),
                sub_type: KnowledgeSubType::Preference,
                confidence: 0.8,
                source_episode_id: None,
                embedding: Some(const_emb(1.0)),
                status: NodeStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("conflict_group_id".to_string(), serde_json::Value::String(group_id.clone()));
                    m
                },
            };
            let node_b = KnowledgeNode {
                id: None,
                subject: "user".to_string(),
                predicate: format!("prefers_{}", i),
                object: "B".to_string(),
                sub_type: KnowledgeSubType::Preference,
                confidence: 0.8,
                source_episode_id: None,
                embedding: Some(flipped_emb(40)), // cos_sim ≈ 0.792 < 0.95 dedup threshold
                status: NodeStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("conflict_group_id".to_string(), serde_json::Value::String(group_id));
                    m
                },
            };
            store.store_knowledge(&node_a).unwrap();
            store.store_knowledge(&node_b).unwrap();
        }

        assert!(store.should_trigger_confirmation().unwrap());
    }

    // =====================================================================
    // Test 4: generate_confirmation_hint
    // =====================================================================

    #[test]
    fn test_generate_confirmation_hint() {
        let store = test_store();

        // No conflicts → None.
        assert!(store.generate_confirmation_hint().unwrap().is_none());

        // Create one ambiguous conflict.
        let two_days_ago = Utc::now() - chrono::TimeDelta::days(2);
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "dark mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: two_days_ago,
            updated_at: two_days_ago,
            metadata: std::collections::HashMap::new(),
        };
        store.store_knowledge(&existing).unwrap();

        let input = crate::consolidation::MemoryStoreInput {
            content: "User prefers light mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: Some("light mode".to_string()),
            confidence: Some(0.88),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };
        store.process_memory_store(&input).unwrap();

        let hint = store.generate_confirmation_hint().unwrap();
        assert!(hint.is_some());
        let text = hint.unwrap();
        assert!(text.contains("ambiguous memory conflicts"));
        assert!(text.contains("dark mode"));
        assert!(text.contains("light mode"));
    }

    // =====================================================================
    // Test 5: resolve_ambiguous keeps chosen node, demotes other
    // =====================================================================

    #[test]
    fn test_resolve_ambiguous() {
        let store = test_store();

        let two_days_ago = Utc::now() - chrono::TimeDelta::days(2);
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "dark mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: two_days_ago,
            updated_at: two_days_ago,
            metadata: std::collections::HashMap::new(),
        };
        let existing_id = store.store_knowledge(&existing).unwrap();

        let input = crate::consolidation::MemoryStoreInput {
            content: "User prefers light mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: Some("light mode".to_string()),
            confidence: Some(0.88),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };
        let result = store.process_memory_store(&input).unwrap();
        let new_id = result.unwrap().node_id;

        let pending = store.get_pending_ambiguous_conflicts().unwrap();
        assert_eq!(pending.len(), 1);
        let group_id = pending[0].conflict_group_id.clone();

        // Resolve: keep the new node.
        store.resolve_ambiguous(&group_id, new_id).unwrap();

        let kept = store.get_knowledge(new_id).unwrap().unwrap();
        assert_eq!(kept.status, NodeStatus::Active);
        assert!(!kept.metadata.contains_key("conflict_group_id"));

        let demoted = store.get_knowledge(existing_id).unwrap().unwrap();
        assert_eq!(demoted.status, NodeStatus::Dormant);
        assert!(!demoted.metadata.contains_key("conflict_group_id"));
    }
}
