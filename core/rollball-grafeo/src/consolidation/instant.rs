//! Instant extraction — real-time processing of `memory_store` tool calls.
//!
//! When the LLM emits a `memory_store` tool call with natural language content,
//! this module handles the full lifecycle: embedding-based dedup, three-layer
//! conflict detection, and status assignment (Active / Pending).

use chrono::Utc;
use grafeo_common::types::NodeId;
use rollball_memory::ConflictSignal;

use crate::conflict::{self, FACT_THRESHOLD, PREFERENCE_THRESHOLD, RELATION_THRESHOLD};
use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{labels, KnowledgeNode, KnowledgeSubType, NodeStatus};

// ---------------------------------------------------------------------------
// Cosine similarity (local copy — semantic/knowledge.rs keeps its own private)
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two embedding vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    let norm_a: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Cosine-similarity threshold above which two knowledge nodes are
/// considered duplicates (identical meaning).
const DEDUP_THRESHOLD: f32 = 0.95;

/// Confidence threshold above which a node is created directly as Active.
const DIRECT_ACTIVE_THRESHOLD: f32 = 0.85;

/// Default confidence assigned when the LLM does not provide one.
const DEFAULT_CONFIDENCE: f32 = 0.7;

// ---------------------------------------------------------------------------
// Input type
// ---------------------------------------------------------------------------

/// Input from LLM's `memory_store` tool call.
#[derive(Debug, Clone)]
pub struct MemoryStoreInput {
    /// Natural language content from LLM.
    pub content: String,
    /// Knowledge sub-type: Fact | Preference | Relation.
    pub sub_type: KnowledgeSubType,
    /// Optional subject hint (defaults to "user").
    pub subject: Option<String>,
    /// Optional predicate hint.
    pub predicate: Option<String>,
    /// Optional object hint.
    pub object: Option<String>,
    /// LLM's confidence in this knowledge (default 0.7).
    pub confidence: Option<f32>,
    /// Source episode ID for traceability.
    pub source_episode_id: Option<NodeId>,
    /// Pre-computed embedding vector.
    pub embedding: Option<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// Conflict candidate
// ---------------------------------------------------------------------------

/// A candidate conflict found during instant extraction.
#[derive(Debug, Clone)]
pub struct ConflictCandidate {
    /// The existing node that conflicts with the new input.
    pub existing_node_id: NodeId,
    /// Conflict signal details from the three-layer detector.
    pub conflict_signal: ConflictSignal,
}

// ---------------------------------------------------------------------------
// Process result
// ---------------------------------------------------------------------------

/// Detailed record of a single conflict resolution action.
#[derive(Debug, Clone)]
pub struct ConflictResolutionDetail {
    /// The existing node involved in the conflict.
    pub existing_node_id: NodeId,
    /// The resolution action taken.
    pub action: crate::conflict::ConflictAction,
    /// The conflict signal that triggered the resolution.
    pub signal: ConflictSignal,
}

/// Result of processing a `memory_store` tool call.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// The ID of the newly created (or updated) knowledge node.
    pub node_id: NodeId,
    /// Detailed conflict resolution records.
    pub conflict_resolutions: Vec<ConflictResolutionDetail>,
}

// ---------------------------------------------------------------------------
// GrafeoStore methods
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Process a `memory_store` tool call from the LLM.
    ///
    /// Pipeline:
    /// 1. If embedding is available, check for duplicates (sim > 0.95 → skip).
    /// 2. If embedding is available, check for conflicts (two-layer heuristic).
    /// 3. Create node with status = Active if confidence >= 0.85, else Pending.
    /// 4. If conflicts detected:
    ///    - All heuristic conflicts are Ambiguous → both Active, mark conflict_group_id.
    ///    - Phase 3 LLM arbitration reclassifies (Evolution / Correction) later.
    ///
    /// Returns the created/updated node ID, or `None` if a duplicate was skipped.
    pub fn process_memory_store(&self, input: &MemoryStoreInput) -> Result<Option<ProcessResult>> {
        let confidence = input.confidence.unwrap_or(DEFAULT_CONFIDENCE);

        // Step 1: Dedup check (only if embedding is available).
        if let Some(ref embedding) = input.embedding
            && self.is_duplicate_knowledge(embedding, DEDUP_THRESHOLD)?
        {
            return Ok(None);
        }

        // Step 2: Conflict detection.
        let conflicts = self.detect_knowledge_conflicts(input)?;

        // Step 3: Determine initial status based on confidence.
        let status = if confidence >= DIRECT_ACTIVE_THRESHOLD {
            NodeStatus::Active
        } else {
            NodeStatus::Pending
        };

        // Step 4: Create the new knowledge node.
        let subject = input
            .subject
            .clone()
            .unwrap_or_else(|| "user".to_string());
        let predicate = input.predicate.clone().unwrap_or_default();
        let object = input.object.clone().unwrap_or_else(|| input.content.clone());

        let mut new_node = KnowledgeNode {
            id: None,
            subject,
            predicate,
            object,
            sub_type: input.sub_type.clone(),
            confidence,
            source_episode_id: input.source_episode_id,
            embedding: input.embedding.clone(),
            status: status.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
        };

        // All heuristic conflicts are Ambiguous — new node keeps its
        // determined status (Active if high confidence, Pending otherwise).
        // Phase 3 LLM arbitration reclassifies and may promote later.

        let new_id = self.store_knowledge(&new_node)?;
        new_node.id = Some(new_id);

        // Step 5: Handle conflict resolution on existing nodes.
        // All heuristic conflicts are Ambiguous — both nodes stay Active
        // with a shared conflict_group_id.  Phase 3 LLM arbitration
        // reclassifies and may demote old nodes to Dormant later.
        let mut conflict_resolutions = Vec::new();
        for conflict in &conflicts {
            let resolution = crate::conflict::resolve_conflict(&conflict.conflict_signal, conflict.existing_node_id);

            // Tag both the existing and new node with the same conflict_group_id.
            let group_id = format!("cg_{}", new_id.as_u64());
            if let Some(mut old_node) = self.get_knowledge(conflict.existing_node_id)? {
                old_node
                    .metadata
                    .insert("conflict_group_id".to_string(), serde_json::Value::String(group_id.clone()));
                old_node.updated_at = Utc::now();
                self.update_knowledge(&old_node)?;
            }
            let mut updated_new = new_node.clone();
            updated_new
                .metadata
                .insert("conflict_group_id".to_string(), serde_json::Value::String(group_id));
            updated_new.updated_at = Utc::now();
            self.update_knowledge(&updated_new)?;

            conflict_resolutions.push(ConflictResolutionDetail {
                existing_node_id: conflict.existing_node_id,
                action: resolution.action,
                signal: conflict.conflict_signal.clone(),
            });
        }

        Ok(Some(ProcessResult {
            node_id: new_id,
            conflict_resolutions,
        }))
    }

    /// Check if a similar knowledge node already exists (dedup).
    ///
    /// Returns `true` if embedding cosine similarity > `threshold` with any
    /// existing Knowledge node.
    pub fn is_duplicate_knowledge(&self, embedding: &[f32], threshold: f32) -> Result<bool> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        for id in node_ids {
            if let Some(n) = self.db.get_node(id)
                && let Some(existing_emb) = n
                    .get_property("embedding")
                    .and_then(|v| v.as_vector().map(|s| s.to_vec()))
            {
                let sim = cosine_similarity(embedding, &existing_emb) as f32;
                if sim > threshold {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Check for conflicting knowledge nodes.
    ///
    /// Uses the three-layer conflict detection from [`conflict::detect_conflict`].
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
                        let existing_created = chrono::DateTime::from_timestamp_micros(ts.as_micros())
                            .unwrap_or_else(Utc::now);
                        let diff = Utc::now() - existing_created;
                        diff.num_seconds() as f64 / 3600.0
                    })
                    .unwrap_or(0.0);

                // Run heuristic conflict detection.
                if let Some(signal) = conflict::detect_conflict(
                    semantic_score,
                    threshold,
                    time_diff_hours,
                ) {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_EMBEDDING_DIM;

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

    // =====================================================================
    // Test 1: Normal creation — high confidence → Active
    // =====================================================================

    #[test]
    fn test_process_memory_store_high_confidence_active() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Beijing".to_string()),
            confidence: Some(0.95),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let id = result.unwrap().node_id;
        let node = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Active);
        assert_eq!(node.subject, "user");
        assert_eq!(node.predicate, "lives_in");
        assert_eq!(node.object, "Beijing");
    }

    // =====================================================================
    // Test 2: Low confidence → Pending
    // =====================================================================

    #[test]
    fn test_process_memory_store_low_confidence_pending() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User might like coffee".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("likes".to_string()),
            object: Some("coffee".to_string()),
            confidence: Some(0.6),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let id = result.unwrap().node_id;
        let node = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Pending);
    }

    // =====================================================================
    // Test 3: Default confidence (0.7) → Pending
    // =====================================================================

    #[test]
    fn test_process_memory_store_default_confidence_pending() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User prefers dark mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: None,
            predicate: None,
            object: None,
            confidence: None, // defaults to 0.7
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let id = result.unwrap().node_id;
        let node = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Pending);
        assert_eq!(node.subject, "user"); // default subject
    }

    // =====================================================================
    // Test 4: Dedup — identical embedding → skip
    // =====================================================================

    #[test]
    fn test_process_memory_store_dedup_skip() {
        let store = test_store();

        // First store.
        let input1 = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Beijing".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };
        let id1 = store.process_memory_store(&input1).unwrap();
        assert!(id1.is_some());
        let _id1 = id1.unwrap().node_id;

        // Second store with same embedding → should be skipped as duplicate.
        let input2 = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Beijing".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)), // identical direction
        };
        let id2 = store.process_memory_store(&input2).unwrap();
        assert!(id2.is_none(), "duplicate should return None");
    }

    // =====================================================================
    // Test 5: Near-duplicate (sim > 0.95) → skip
    // =====================================================================

    #[test]
    fn test_process_memory_store_near_duplicate_skip() {
        let store = test_store();

        let input1 = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Beijing".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };
        store.process_memory_store(&input1).unwrap();

        // Embedding with cos_sim ≈ 0.953 (flip 9) → just above dedup threshold.
        let input2 = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(flipped_emb(9)), // cos ≈ 0.953 > 0.95
        };
        let id2 = store.process_memory_store(&input2).unwrap();
        assert!(id2.is_none(), "near-duplicate should return None");
    }

    // =====================================================================
    // Test 6: No embedding → skip dedup/conflict, create directly
    // =====================================================================

    #[test]
    fn test_process_memory_store_no_embedding() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User likes tea".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("likes".to_string()),
            object: Some("tea".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: None,
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let node = store.get_knowledge(result.unwrap().node_id).unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Active);
        assert!(node.embedding.is_none());
    }

    // =====================================================================
    // Test 7: Conflict — now always Ambiguous (old stays Active, conflict_group_id)
    // =====================================================================

    #[test]
    fn test_process_memory_store_conflict_correction() {
        let store = test_store();

        // Create an existing knowledge node directly.
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        let existing_id = store.store_knowledge(&existing).unwrap();

        // New input — used to trigger Correction; now all Ambiguous.
        let input = MemoryStoreInput {
            content: "User actually lives in Shanghai, not Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.95),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let new_id = result.unwrap().node_id;

        // Old node stays Active (Ambiguous — no auto-demotion).
        let old_node = store.get_knowledge(existing_id).unwrap().unwrap();
        assert_eq!(old_node.status, NodeStatus::Active);

        // New node is Active because confidence ≥ 0.85.
        let new_node = store.get_knowledge(new_id).unwrap().unwrap();
        assert_eq!(new_node.status, NodeStatus::Active);
        assert_eq!(new_node.object, "Shanghai");

        // Both tagged with conflict_group_id.
        assert!(old_node.metadata.contains_key("conflict_group_id"));
        assert!(new_node.metadata.contains_key("conflict_group_id"));
    }

    // =====================================================================
    // Test 8: Conflict — Ambiguous → both Active, conflict_group_id
    // =====================================================================

    #[test]
    fn test_process_memory_store_conflict_ambiguous() {
        let store = test_store();

        // Create an existing knowledge node (old but not too old, 2 days ago).
        // Time diff ~2 days → no temporal conflict, not old enough for evolution.
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

        // New input with neutral content (no negation, no evolution keywords,
        // but semantically similar enough to trigger ambiguous conflict).
        // Preference threshold = 0.80, so cos ≈ 0.922 > 0.80 triggers conflict.
        let input = MemoryStoreInput {
            content: "User prefers light mode".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: Some("light mode".to_string()),
            confidence: Some(0.88),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)), // cos ≈ 0.922 with const_emb(1.0)
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let new_id = result.unwrap().node_id;

        // Both should be Active.
        let old_node = store.get_knowledge(existing_id).unwrap().unwrap();
        assert_eq!(old_node.status, NodeStatus::Active);

        let new_node = store.get_knowledge(new_id).unwrap().unwrap();
        assert_eq!(new_node.status, NodeStatus::Active);

        // Both should have conflict_group_id in metadata.
        assert!(old_node.metadata.contains_key("conflict_group_id"));
        assert!(new_node.metadata.contains_key("conflict_group_id"));
        assert_eq!(
            old_node.metadata.get("conflict_group_id"),
            new_node.metadata.get("conflict_group_id"),
        );
    }

    // =====================================================================
    // Test 9: is_duplicate_knowledge — true when similar exists
    // =====================================================================

    #[test]
    fn test_is_duplicate_knowledge_true() {
        let store = test_store();

        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "name".to_string(),
            object: "Alice".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_knowledge(&node).unwrap();

        // Same direction → cosine sim = 1.0 > 0.95.
        let is_dup = store.is_duplicate_knowledge(&const_emb(1.0), 0.95).unwrap();
        assert!(is_dup, "identical embedding should be detected as duplicate");
    }

    // =====================================================================
    // Test 10: is_duplicate_knowledge — false when no similar exists
    // =====================================================================

    #[test]
    fn test_is_duplicate_knowledge_false() {
        let store = test_store();

        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "name".to_string(),
            object: "Alice".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_knowledge(&node).unwrap();

        // Different direction (flip 40 → cos ≈ 0.792 < 0.95) → not duplicate.
        let is_dup = store.is_duplicate_knowledge(&flipped_emb(40), 0.95).unwrap();
        assert!(!is_dup, "different embedding should not be duplicate");
    }

    // =====================================================================
    // Test 11: detect_knowledge_conflicts returns candidates
    // =====================================================================

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
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: test_dt(),
            updated_at: test_dt(),
            metadata: std::collections::HashMap::new(),
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
        };

        let conflicts = store.detect_knowledge_conflicts(&input).unwrap();
        assert!(!conflicts.is_empty(), "should detect conflict with similar node");
    }

    // =====================================================================
    // Test 12: detect_knowledge_conflicts — no embedding → empty
    // =====================================================================

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
        };

        let conflicts = store.detect_knowledge_conflicts(&input).unwrap();
        assert!(conflicts.is_empty());
    }

    // =====================================================================
    // Test 13: Content used as object fallback when object is None
    // =====================================================================

    #[test]
    fn test_process_memory_store_content_as_object_fallback() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "User prefers concise answers".to_string(),
            sub_type: KnowledgeSubType::Preference,
            subject: Some("user".to_string()),
            predicate: Some("prefers".to_string()),
            object: None, // should fall back to content
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };

        let result = store.process_memory_store(&input).unwrap();
        let node = store.get_knowledge(result.unwrap().node_id).unwrap().unwrap();
        assert_eq!(node.object, "User prefers concise answers");
    }

    // =====================================================================
    // Test 14: Conflict — old node stays Active (Ambiguous, no auto-demotion)
    // =====================================================================

    #[test]
    fn test_process_memory_store_conflict_evolution() {
        let store = test_store();

        // Create an existing node far in the past (10 days old).
        let old_time = Utc::now() - chrono::TimeDelta::days(10);
        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
        };
        let existing_id = store.store_knowledge(&existing).unwrap();

        let input = MemoryStoreInput {
            content: "User moved to Shanghai".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.95),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let new_id = result.unwrap().node_id;

        // Old node stays Active (Ambiguous — Phase 3 LLM will decide).
        let old_node = store.get_knowledge(existing_id).unwrap().unwrap();
        assert_eq!(old_node.status, NodeStatus::Active);
        assert!(old_node.metadata.contains_key("conflict_group_id"));

        // New node is Active (confidence ≥ 0.85).
        let new_node = store.get_knowledge(new_id).unwrap().unwrap();
        assert_eq!(new_node.status, NodeStatus::Active);
        assert!(new_node.metadata.contains_key("conflict_group_id"));
    }

    // =====================================================================
    // Test 15: Conflict — no edges created (deferred to Phase 3 LLM)
    // =====================================================================

    #[test]
    fn test_process_memory_store_no_auto_edges() {
        let store = test_store();

        let existing = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "lives_in".to_string(),
            object: "Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        let existing_id = store.store_knowledge(&existing).unwrap();

        let input = MemoryStoreInput {
            content: "User actually lives in Shanghai, not Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.95),
            source_episode_id: None,
            embedding: Some(flipped_emb(15)),
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let new_id = result.unwrap().node_id;

        // No auto CORRECTS or EVOLUTION_FROM edges — Phase 3 LLM creates them.
        let edges = store.get_edges(new_id, grafeo_core::graph::Direction::Outgoing);
        let has_corrects = edges.iter().any(|e| {
            e.edge_type == "CORRECTS" && e.dst == existing_id
        });
        let has_evolution = edges.iter().any(|e| {
            e.edge_type == "EVOLUTION_FROM" && e.dst == existing_id
        });
        assert!(!has_corrects, "CORRECTS edge should NOT be auto-created");
        assert!(!has_evolution, "EVOLUTION_FROM edge should NOT be auto-created");
    }
}
