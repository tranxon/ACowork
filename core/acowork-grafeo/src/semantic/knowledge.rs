//! KnowledgeNode storage with semantic deduplication.

use chrono::Utc;
use grafeo_common::types::{NodeId, Value};

use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{KnowledgeNode, labels};

/// Cosine-similarity threshold for treating two facts as identical.
const DEDUP_SIMILARITY_THRESHOLD: f64 = 0.95;

/// Compute cosine similarity between two embedding vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm_a: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

impl GrafeoStore {
    /// Store a knowledge node.
    ///
    /// Performs semantic deduplication: if an existing node shares the same
    /// `(subject, predicate)` and embedding cosine similarity is greater than
    /// `0.95`, the existing node is updated instead of creating a duplicate.
    pub fn store_knowledge(&self, node: &KnowledgeNode) -> Result<NodeId> {
        // If the node already carries an ID, treat it as an explicit update.
        if let Some(id) = node.id {
            return self.update_knowledge(node).map(|_| id);
        }

        // Look for an existing node with the same (subject, predicate).
        if let Some(existing) = self.find_knowledge_by_subject(&node.subject, &node.predicate)? {
            // Compare embeddings when both are present.
            let should_merge = match (&node.embedding, &existing.embedding) {
                (Some(new_emb), Some(old_emb)) => {
                    cosine_similarity(new_emb, old_emb) > DEDUP_SIMILARITY_THRESHOLD
                }
                // If embeddings are missing on either side, fall back to
                // subject+predicate equality (conservative dedup).
                _ => true,
            };

            if should_merge {
                let merged = KnowledgeNode {
                    id: existing.id,
                    object: node.object.clone(),
                    confidence: node.confidence,
                    updated_at: Utc::now(),
                    embedding: node.embedding.clone().or(existing.embedding.clone()),
                    source_episode_id: node.source_episode_id.or(existing.source_episode_id),
                    ..existing
                };
                self.update_knowledge(&merged)?;
                return Ok(merged.id.unwrap());
            }
        }

        // No duplicate found — create a fresh node.
        let props = node.to_properties();
        let id = self.store_node(
            labels::KNOWLEDGE,
            props.iter().map(|(k, v)| (k.as_str(), v.clone())),
        )?;
        Ok(id)
    }

    /// Find an existing knowledge node by exact `subject` + `predicate` match.
    pub fn find_knowledge_by_subject(
        &self,
        subject: &str,
        predicate: &str,
    ) -> Result<Option<KnowledgeNode>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                let subject_match = n
                    .get_property("subject")
                    .and_then(Value::as_str)
                    .map(|s| s == subject)
                    .unwrap_or(false);
                let predicate_match = n
                    .get_property("predicate")
                    .and_then(Value::as_str)
                    .map(|p| p == predicate)
                    .unwrap_or(false);

                if subject_match && predicate_match {
                    let props: Vec<(String, Value)> = n
                        .properties_as_btree()
                        .into_iter()
                        .map(|(k, v)| (k.as_str().to_string(), v))
                        .collect();
                    return Ok(Some(KnowledgeNode::from_properties(id, &props)?));
                }
            }
        }
        Ok(None)
    }

    /// Retrieve a knowledge node by its Grafeo ID.
    pub fn get_knowledge(&self, id: NodeId) -> Result<Option<KnowledgeNode>> {
        match self.db.get_node(id) {
            Some(n) => {
                let props: Vec<(String, Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                Ok(Some(KnowledgeNode::from_properties(id, &props)?))
            }
            None => Ok(None),
        }
    }

    /// Update fields of an existing knowledge node.
    pub fn update_knowledge(&self, node: &KnowledgeNode) -> Result<()> {
        let id = node.id.ok_or_else(|| {
            crate::error::GrafeoError::Memory(
                "cannot update knowledge node without an ID".to_string(),
            )
        })?;

        let props = node.to_properties();
        self.update_node(id, props.iter().map(|(k, v)| (k.as_str(), v.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KnowledgeSubType, NodeStatus};

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn emb(v: f32) -> Vec<f32> {
        vec![v; crate::types::DEFAULT_EMBEDDING_DIM]
    }

    #[test]
    fn test_store_knowledge_new() {
        let store = test_store();
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(emb(0.1)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };

        let id = store.store_knowledge(&node).unwrap();
        let fetched = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(fetched.subject, "user");
        assert_eq!(fetched.predicate, "lives_in");
        assert_eq!(fetched.object, "Beijing");
    }

    #[test]
    fn test_store_knowledge_dedup() {
        let store = test_store();
        let node1 = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "prefers".to_string(),
            object: "concise".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(emb(0.5)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };

        let id1 = store.store_knowledge(&node1).unwrap();

        // Second store with nearly identical embedding should dedup.
        let node2 = KnowledgeNode {
            id: None,
            object: "very concise".to_string(),
            confidence: 0.95,
            embedding: Some(emb(0.51)), // similarity ~0.99
            ..node1.clone()
        };
        let id2 = store.store_knowledge(&node2).unwrap();

        assert_eq!(id1, id2, "dedup should reuse the same node ID");

        let fetched = store.get_knowledge(id1).unwrap().unwrap();
        assert_eq!(fetched.object, "very concise");
        assert!((fetched.confidence - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn test_find_knowledge_by_subject() {
        let store = test_store();
        let node = KnowledgeNode {
            id: None,
            subject: "agent".to_string(),
            predicate: "name".to_string(),
            object: "WeatherBot".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 1.0,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_knowledge(&node).unwrap();

        let found = store
            .find_knowledge_by_subject("agent", "name")
            .unwrap()
            .unwrap();
        assert_eq!(found.object, "WeatherBot");

        let missing = store.find_knowledge_by_subject("agent", "age").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_update_knowledge() {
        let store = test_store();
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "language".to_string(),
            object: "Chinese".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        let id = store.store_knowledge(&node).unwrap();

        let updated = KnowledgeNode {
            id: Some(id),
            object: "English".to_string(),
            confidence: 0.99,
            ..node
        };
        store.update_knowledge(&updated).unwrap();

        let fetched = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(fetched.object, "English");
        assert!((fetched.confidence - 0.99).abs() < f32::EPSILON);
    }
}
