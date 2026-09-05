//! Landing-time knowledge dedup & conflict helpers.
//!
//! ADR-068 §3.3 removed the direct LLM→sediment write pipeline that used to
//! live in this module (the `memory_store` instant writer and its
//! autobiographical / procedure branches). The LLM-side `memory_store` tool
//! now writes Episodes only; promotion into the sediment layer is owned
//! exclusively by [`crate::consolidation::distiller::EpisodicDistiller`].
//!
//! What remains here are the embedding-based helpers that the rest of the
//! landing/consolidation stack still calls:
//!
//! - [`GrafeoStore::is_duplicate_knowledge`] — dedup check used by triple
//!   ingestion before it creates KnowledgeNodes;
//! - [`GrafeoStore::detect_knowledge_conflicts`] — heuristic conflict scan
//!   feeding the ambiguous-conflict confirmation flow.
//!
//! These helpers are intentionally pure checks: they read the graph and
//! return candidates, they never write sediment nodes directly.

use acowork_memory::ConflictSignal;
use chrono::Utc;
use grafeo_common::types::{NodeId, Value};

use crate::conflict::{
    self, FACT_THRESHOLD, PREFERENCE_THRESHOLD, PROCEDURE_THRESHOLD, RELATION_THRESHOLD,
};
use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{KnowledgeSubType, labels};

// ---------------------------------------------------------------------------
// Cosine similarity (local copy — semantic/knowledge.rs keeps its own private)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Types (re-exported from acowork-memory)
// ---------------------------------------------------------------------------

pub use acowork_memory::consolidation::{
    ConflictAction, ConflictResolutionDetail, MemoryStoreInput, MemoryStoreResult,
};
/// Backward-compatible alias.
pub use acowork_memory::consolidation::MemoryStoreResult as ProcessResult;

// ---------------------------------------------------------------------------
// Conflict candidate (grafeo-internal, uses NodeId)
// ---------------------------------------------------------------------------

/// A candidate conflict found during landing-time dedup/conflict scans.
#[derive(Debug, Clone)]
pub struct ConflictCandidate {
    /// The existing node that conflicts with the new input.
    pub existing_node_id: NodeId,
    /// Conflict signal details from the heuristic detector.
    pub conflict_signal: ConflictSignal,
}

// ---------------------------------------------------------------------------
// GrafeoStore methods
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Check if a similar knowledge node already exists (dedup).
    ///
    /// Two dedup dimensions:
    /// 1. **Semantic**: embedding cosine similarity > `threshold`
    /// 2. **Structured**: `(subject, predicate)` exact match
    ///
    /// A node is considered a duplicate ONLY if both dimensions match
    /// AND the object also matches (or no object comparison is possible).
    /// If (subject, predicate) matches but object differs, this is a
    /// knowledge update (not a duplicate) — the caller should route it
    /// to conflict detection instead.
    ///
    /// Returns the ID of the most similar duplicate if found, or `None`.
    pub fn is_duplicate_knowledge(
        &self,
        embedding: &[f32],
        threshold: f32,
        subject: Option<&str>,
        predicate: Option<&str>,
        object: Option<&str>,
    ) -> Result<Option<NodeId>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                // Check semantic similarity.
                let Some(existing_emb) = n
                    .get_property("embedding")
                    .and_then(|v| v.as_vector().map(|s| s.to_vec()))
                else {
                    continue;
                };
                let sim = cosine_similarity(embedding, &existing_emb) as f32;
                if sim <= threshold {
                    continue; // Not semantically similar enough.
                }

                // Semantic match found. Now check structured match.
                match (subject, predicate) {
                    (Some(subj), Some(pred)) => {
                        let existing_subject = n
                            .get_property("subject")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let existing_predicate = n
                            .get_property("predicate")
                            .and_then(Value::as_str)
                            .unwrap_or("");

                        if !existing_subject.eq_ignore_ascii_case(subj)
                            || !existing_predicate.eq_ignore_ascii_case(pred)
                        {
                            // Different (subject, predicate) → not structured match.
                            // Keep looking for a better match.
                            continue;
                        }

                        // Same (subject, predicate). Check if object also matches.
                        let existing_object = n
                            .get_property("object")
                            .and_then(Value::as_str)
                            .unwrap_or("");

                        // If we have an object in the input AND the existing
                        // node has an object, compare them.
                        if let Some(obj) = object
                            && !existing_object.eq_ignore_ascii_case(obj) {
                                // Same (subject, predicate), different object →
                                // this is a knowledge UPDATE, not a duplicate.
                                // Return None so the caller proceeds to conflict
                                // detection, which will handle the update.
                                return Ok(None);
                            }

                        // Same (subject, predicate) and same (or absent) object →
                        // true duplicate.
                        return Ok(Some(id));
                    }
                    _ => {
                        // No structured fields provided — fall back to pure
                        // semantic dedup (original behavior).
                        return Ok(Some(id));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Check for conflicting knowledge nodes.
    ///
    /// Uses the heuristic conflict detection from [`conflict::detect_conflict`].
    /// Scans all existing Knowledge nodes and returns candidates whose semantic
    /// similarity exceeds the sub-type-specific threshold.
    pub fn detect_knowledge_conflicts(
        &self,
        input: &MemoryStoreInput,
    ) -> Result<Vec<ConflictCandidate>> {
        let embedding = match &input.embedding {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let threshold = match input.sub_type {
            KnowledgeSubType::Fact => FACT_THRESHOLD,
            KnowledgeSubType::Preference => PREFERENCE_THRESHOLD,
            KnowledgeSubType::Relation => RELATION_THRESHOLD,
            KnowledgeSubType::Procedure => PROCEDURE_THRESHOLD,
        };

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        let mut candidates = Vec::new();

        for id in node_ids {
            if let Some(n) = self.db.get_node(id)
                && let Some(existing_emb) = n
                    .get_property("embedding")
                    .and_then(|v| v.as_vector().map(|s| s.to_vec()))
            {
                let semantic_score = cosine_similarity(embedding, &existing_emb) as f32;

                // Quick skip: below threshold.
                if semantic_score < threshold {
                    continue;
                }

                // Extract created_at timestamp for temporal conflict detection.
                let time_diff_hours = n
                    .get_property("created_at")
                    .and_then(|v| v.as_timestamp())
                    .map(|ts| {
                        let existing_created =
                            chrono::DateTime::from_timestamp_micros(ts.as_micros())
                                .unwrap_or_else(Utc::now);
                        let diff = Utc::now() - existing_created;
                        diff.num_seconds() as f64 / 3600.0
                    })
                    .unwrap_or(0.0);

                // Run heuristic conflict detection.
                if let Some(signal) =
                    conflict::detect_conflict(semantic_score, threshold, time_diff_hours)
                {
                    candidates.push(ConflictCandidate {
                        existing_node_id: id,
                        conflict_signal: signal,
                    });
                }
            }
        }

        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        DEFAULT_EMBEDDING_DIM, KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel,
    };

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    /// Create a constant-value embedding (all elements same).
    /// NOTE: All constant vectors have cosine similarity = 1.0 regardless of value.
    fn const_emb(v: f32) -> Vec<f32> {
        vec![v; DEFAULT_EMBEDDING_DIM]
    }

    /// Create an embedding that has a controlled cosine similarity to `const_emb(1.0)`.
    ///
    /// Strategy: flip the sign of the last `flip_count` elements.
    /// cos_sim = (N - 2*flip_count) / N
    /// flip  0 → cos = 1.000  (identical)
    /// flip  9 → cos ≈ 0.953  (just above dedup 0.95)
    /// flip 10 → cos ≈ 0.948  (just below dedup 0.95)
    /// flip 15 → cos ≈ 0.922  (above fact conflict 0.85, below dedup 0.95)
    /// flip 28 → cos ≈ 0.854  (just above fact conflict 0.85)
    /// flip 40 → cos ≈ 0.792  (below conflict 0.85)
    fn flipped_emb(flip_count: usize) -> Vec<f32> {
        let mut v = vec![1.0f32; DEFAULT_EMBEDDING_DIM];
        for i in 0..flip_count {
            v[DEFAULT_EMBEDDING_DIM - 1 - i] = -1.0;
        }
        v
    }

    fn sample_node(subject: &str, object: &str) -> KnowledgeNode {
        KnowledgeNode {
            id: None,
            subject: subject.to_string(),
            predicate: "name".to_string(),
            object: object.to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        }
    }

    #[test]
    fn test_is_duplicate_knowledge_true() {
        let store = test_store();
        store.store_knowledge(&sample_node("user", "Alice")).unwrap();

        // Same direction → cosine sim = 1.0 > 0.95.
        // Same subject+predicate+object → true duplicate.
        let is_dup = store
            .is_duplicate_knowledge(
                &const_emb(1.0),
                0.95,
                Some("user"),
                Some("name"),
                Some("Alice"),
            )
            .unwrap();
        assert!(
            is_dup.is_some(),
            "identical embedding + same (subject, predicate, object) should be duplicate"
        );

        // Same direction but different object → knowledge update, NOT duplicate.
        let is_dup = store
            .is_duplicate_knowledge(
                &const_emb(1.0),
                0.95,
                Some("user"),
                Some("name"),
                Some("Bob"),
            )
            .unwrap();
        assert!(
            is_dup.is_none(),
            "same (subject, predicate) but different object should NOT be duplicate"
        );
    }

    #[test]
    fn test_is_duplicate_knowledge_false() {
        let store = test_store();
        store.store_knowledge(&sample_node("user", "Alice")).unwrap();

        // Different direction (flip 40 → cos ≈ 0.792 < 0.95) → not duplicate.
        let is_dup = store
            .is_duplicate_knowledge(
                &flipped_emb(40),
                0.95,
                Some("user"),
                Some("name"),
                Some("Alice"),
            )
            .unwrap();
        assert!(
            is_dup.is_none(),
            "different embedding should not be duplicate"
        );
    }

    #[test]
    fn test_detect_knowledge_conflicts_returns_candidates() {
        let store = test_store();

        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&existing).unwrap();

        // Embedding with cos ≈ 0.922 (flip 15) → above fact threshold 0.85.
        let input = MemoryStoreInput {
            content: "User lives in Shanghai now".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.95),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
            privacy: None,
            importance: None,
            keywords: None,
            autobiographical: None,
        };

        let conflicts = store.detect_knowledge_conflicts(&input).unwrap();
        assert!(
            !conflicts.is_empty(),
            "should detect conflict with similar node"
        );
    }

    #[test]
    fn test_detect_knowledge_conflicts_no_embedding() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User likes tea".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: None,
            predicate: None,
            object: None,
            confidence: None,
            source_episode_id: None,
            embedding: None,
            privacy: None,
            importance: None,
            keywords: None,
            autobiographical: None,
        };

        let conflicts = store.detect_knowledge_conflicts(&input).unwrap();
        assert!(conflicts.is_empty());
    }
}
