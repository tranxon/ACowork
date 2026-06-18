//! Instant extraction — real-time processing of `memory_store` tool calls.
//!
//! When the LLM emits a `memory_store` tool call with natural language content,
//! this module handles the full lifecycle: embedding-based dedup, three-layer
//! conflict detection, and status assignment (Active / Pending).

use acowork_memory::ConflictSignal;
use chrono::Utc;
use grafeo_common::types::NodeId;

use crate::conflict::{
    self, FACT_THRESHOLD, PREFERENCE_THRESHOLD, PROCEDURE_THRESHOLD, RELATION_THRESHOLD,
};
use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{KnowledgeNode, KnowledgeSubType, NodeStatus, ProceduralNode, labels};
use grafeo_common::types::Value;

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
// Constants
// ---------------------------------------------------------------------------

/// Cosine-similarity threshold above which two knowledge nodes are
/// considered duplicates (identical meaning).
const DEDUP_THRESHOLD: f32 = 0.95;

/// Cosine-similarity threshold above which two procedural nodes are
/// considered duplicates (same trigger+action). Lower than knowledge
/// dedup because procedures are more specific and slight wording
/// variations still indicate the same behavior pattern.
const PROCEDURE_DEDUP_THRESHOLD: f32 = 0.90;

/// Confidence boost applied when a duplicate procedure reinforces an
/// existing node (instead of creating a new one).
const PROCEDURE_CONFIDENCE_BOOST: f32 = 0.05;

/// Maximum confidence for a ProceduralNode (cap after boosting).
const PROCEDURE_MAX_CONFIDENCE: f32 = 0.98;

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
    /// - **Procedure category**: creates a `ProceduralNode` directly (no
    ///   conflict detection — procedures don't conflict with facts).
    /// - **Fact / Preference / Relation**: runs the full pipeline:
    ///   1. If embedding is available, check for duplicates (sim > 0.95 → skip).
    ///   2. If embedding is available, check for conflicts (two-layer heuristic).
    ///   3. Create node with status = Active if confidence >= 0.85, else Pending.
    ///   4. If conflicts detected:
    ///      - All heuristic conflicts are Ambiguous → both Active, mark conflict_group_id.
    ///      - Phase 3 LLM arbitration reclassifies (Evolution / Correction) later.
    ///
    /// Returns the created/updated node ID, or `None` if a duplicate was skipped.
    pub fn process_memory_store(&self, input: &MemoryStoreInput) -> Result<Option<ProcessResult>> {
        let confidence = input.confidence.unwrap_or(DEFAULT_CONFIDENCE);

        // --- Procedure path: create ProceduralNode directly ---
        if matches!(input.sub_type, KnowledgeSubType::Procedure) {
            return self.process_procedure(input, confidence);
        }

        // --- Fact / Preference / Relation path ---

        // Step 1: Dedup check (only if embedding is available).
        // P3 T4.5: Enhanced dedup — pass (subject, predicate, object) for structured
        // matching. If (subject, predicate) matches but object differs, it's
        // a knowledge update (handled by conflict detection in Step 2), not dedup.
        if let Some(ref embedding) = input.embedding
            && self
                .is_duplicate_knowledge(
                    embedding,
                    DEDUP_THRESHOLD,
                    input.subject.as_deref(),
                    input.predicate.as_deref(),
                    input.object.as_deref(),
                )?
                .is_some()
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
        let subject = input.subject.clone().unwrap_or_else(|| "user".to_string());
        let predicate = input.predicate.clone().unwrap_or_default();
        let object = input
            .object
            .clone()
            .unwrap_or_else(|| input.content.clone());

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
            let resolution = crate::conflict::resolve_conflict(
                &conflict.conflict_signal,
                conflict.existing_node_id,
            );

            // Tag both the existing and new node with the same conflict_group_id.
            let group_id = format!("cg_{}", new_id.as_u64());
            if let Some(mut old_node) = self.get_knowledge(conflict.existing_node_id)? {
                old_node.metadata.insert(
                    "conflict_group_id".to_string(),
                    serde_json::Value::String(group_id.clone()),
                );
                old_node.updated_at = Utc::now();
                self.update_knowledge(&old_node)?;
            }
            let mut updated_new = new_node.clone();
            updated_new.metadata.insert(
                "conflict_group_id".to_string(),
                serde_json::Value::String(group_id),
            );
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

    /// Process a `memory_store` tool call with `category="procedure"`.
    ///
    /// Pipeline:
    /// 1. If embedding is available, check for duplicates (sim > 0.90).
    ///    - If duplicate found, boost the existing node's confidence
    ///      instead of creating a new one (reinforcement).
    /// 2. No cross-type conflict detection (procedures don't conflict
    ///    with facts/preferences — they live on a different Label).
    /// 3. Parse content into (trigger_condition, action_pattern).
    /// 4. Create ProceduralNode with status based on confidence.
    fn process_procedure(
        &self,
        input: &MemoryStoreInput,
        confidence: f32,
    ) -> Result<Option<ProcessResult>> {
        // Step 1: Dedup check (only if embedding is available).
        if let Some(ref embedding) = input.embedding
            && let Some(existing_id) =
                self.find_duplicate_procedure(embedding, PROCEDURE_DEDUP_THRESHOLD)?
            {
                // Reinforce the existing node: boost confidence.
                if let Some(mut existing) = self.get_procedural(existing_id)? {
                    let _old_confidence = existing.confidence;
                    existing.confidence = (existing.confidence + PROCEDURE_CONFIDENCE_BOOST)
                        .min(PROCEDURE_MAX_CONFIDENCE);
                    existing.updated_at = Utc::now();
                    self.update_procedural(&existing)?;

                    // Confidence boosted via reinforcement (dedup).
                    return Ok(Some(ProcessResult {
                        node_id: existing_id,
                        conflict_resolutions: Vec::new(),
                    }));
                }
            }

        // Step 2: Parse content into (trigger_condition, action_pattern).
        let (trigger, action) = parse_procedure_content(&input.content);

        // Step 3: Determine initial status based on confidence.
        let status = if confidence >= DIRECT_ACTIVE_THRESHOLD {
            NodeStatus::Active
        } else {
            NodeStatus::Pending
        };

        // Generate a name from the trigger condition (first few words).
        let name = trigger
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join("_")
            .to_lowercase();

        let node = ProceduralNode {
            id: None,
            name,
            trigger_condition: trigger,
            action_pattern: action,
            success_count: 0,
            fail_count: 0,
            confidence,
            activation_count: 0,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: input.embedding.clone(),
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
        };

        let new_id = self.store_procedural(&node)?;

        Ok(Some(ProcessResult {
            node_id: new_id,
            conflict_resolutions: Vec::new(),
        }))
    }

    /// Check if a similar procedural node already exists (dedup).
    ///
    /// Returns the ID of the most similar existing ProceduralNode if
    /// cosine similarity > `threshold`, or `None` if no match.
    fn find_duplicate_procedure(
        &self,
        embedding: &[f32],
        threshold: f32,
    ) -> Result<Option<NodeId>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::PROCEDURAL);

        let mut best: Option<(NodeId, f32)> = None;

        for id in node_ids {
            if let Some(n) = self.db.get_node(id)
                && let Some(existing_emb) = n
                    .get_property("embedding")
                    .and_then(|v| v.as_vector().map(|s| s.to_vec()))
            {
                let sim = cosine_similarity(embedding, &existing_emb) as f32;
                if sim > threshold {
                    match best {
                        Some((_best_id, best_sim)) if sim <= best_sim => {}
                        _ => best = Some((id, sim)),
                    }
                }
            }
        }

        Ok(best.map(|(id, _)| id))
    }

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

// ---------------------------------------------------------------------------
// Procedure content parsing
// ---------------------------------------------------------------------------

/// Parse natural language content into (trigger_condition, action_pattern).
///
/// Heuristic separators (in priority order):
/// 1. "→" or "->" (arrow)
/// 2. "when X, do Y" / "when X, Y" / "if X, Y"
/// 3. Comma-separated imperative: "X, prefer Y"
///
/// If no separator is found, the full content becomes both trigger and
/// action (the runtime will refine via offline consolidation later).
fn parse_procedure_content(content: &str) -> (String, String) {
    // Arrow separator: "when X → do Y" or "when X -> do Y"
    if let Some(pos) = content.find("→").or_else(|| content.find("->")) {
        let trigger = content[..pos].trim();
        let action = content[pos
            + if content.contains("→") {
                '→'.len_utf8()
            } else {
                2
            }..]
            .trim();
        if !trigger.is_empty() && !action.is_empty() {
            return (trigger.to_string(), action.to_string());
        }
    }

    // "when X, do Y" / "if X, Y" pattern
    let lower = content.to_lowercase();
    for prefix in &["when ", "when,", "if ", "whenever "] {
        if lower.starts_with(prefix) {
            let rest = &content[prefix.len()..];
            // Find the comma that separates trigger from action
            if let Some(comma_pos) = rest.find(',') {
                let trigger = rest[..comma_pos].trim();
                let action = rest[comma_pos + 1..]
                    .trim()
                    .trim_start_matches("do ")
                    .trim_start_matches("then ")
                    .trim_start_matches("prefer ")
                    .trim();
                if !trigger.is_empty() && !action.is_empty() {
                    return (trigger.to_string(), action.to_string());
                }
            }
        }
    }

    // Comma separator as last resort: "X, Y"
    if let Some(comma_pos) = content.find(',') {
        let trigger = content[..comma_pos].trim();
        let action = content[comma_pos + 1..].trim();
        if !trigger.is_empty() && !action.is_empty() {
            return (trigger.to_string(), action.to_string());
        }
    }

    // No separator found — use content as both trigger and action.
    (content.to_string(), content.to_string())
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

        // === T4.5: Test that same (subject, predicate) + different object ===
        // is treated as knowledge update, NOT duplicate.

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

        // Same (subject, predicate) but different object → NOT a duplicate.
        // This is a knowledge update, so conflict detection should handle it.
        // Embedding with cos_sim ≈ 0.953 (flip 9) → above dedup threshold.
        let input2 = MemoryStoreInput {
            content: "User lives in Shanghai".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Shanghai".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(flipped_emb(9)), // cos ≈ 0.953 > 0.95
        };
        let id2 = store.process_memory_store(&input2).unwrap();
        // T4.5: Should NOT be None — it's a knowledge update, not a duplicate.
        assert!(
            id2.is_some(),
            "same (subject, predicate) + different object should go through conflict detection"
        );
    }

    #[test]
    fn test_process_memory_store_true_duplicate_skip() {
        let store = test_store();

        // T4.5: Test that identical (subject, predicate, object) + same embedding
        // IS treated as a true duplicate and skipped.

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

        // Exact same triple + identical embedding → true duplicate.
        let input2 = MemoryStoreInput {
            content: "User lives in Beijing".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: Some("user".to_string()),
            predicate: Some("lives_in".to_string()),
            object: Some("Beijing".to_string()),
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };
        let id2 = store.process_memory_store(&input2).unwrap();
        assert!(
            id2.is_none(),
            "identical (subject, predicate, object) + same embedding should be deduplicated"
        );
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
        let node = store
            .get_knowledge(result.unwrap().node_id)
            .unwrap()
            .unwrap();
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
        assert!(
            !conflicts.is_empty(),
            "should detect conflict with similar node"
        );
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
        let node = store
            .get_knowledge(result.unwrap().node_id)
            .unwrap()
            .unwrap();
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
        let has_corrects = edges
            .iter()
            .any(|e| e.edge_type == "CORRECTS" && e.dst == existing_id);
        let has_evolution = edges
            .iter()
            .any(|e| e.edge_type == "EVOLUTION_FROM" && e.dst == existing_id);
        assert!(!has_corrects, "CORRECTS edge should NOT be auto-created");
        assert!(
            !has_evolution,
            "EVOLUTION_FROM edge should NOT be auto-created"
        );
    }

    // =====================================================================
    // Test 16: Procedure category creates ProceduralNode
    // =====================================================================

    #[test]
    fn test_process_memory_store_procedure() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "when user asks for summary, reply in 3 sentences max".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: None,
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.conflict_resolutions.is_empty());

        // Verify it was stored as a ProceduralNode.
        let found = store.find_procedural_by_trigger("summary", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].learned_from, "user_feedback");
        assert_eq!(found[0].status, NodeStatus::Active); // confidence 0.9 ≥ 0.85
    }

    // =====================================================================
    // Test 17: Procedure — low confidence → Pending
    // =====================================================================

    #[test]
    fn test_process_memory_store_procedure_low_confidence() {
        let store = test_store();
        let input = MemoryStoreInput {
            content: "user prefers tables over lists".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.6),
            source_episode_id: None,
            embedding: None,
        };

        let result = store.process_memory_store(&input).unwrap();
        assert!(result.is_some());

        let found = store.find_procedural_by_trigger("tables", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].status, NodeStatus::Pending); // confidence 0.6 < 0.85
    }

    // =====================================================================
    // Test 18: parse_procedure_content — arrow separator
    // =====================================================================

    #[test]
    fn test_parse_procedure_content_arrow() {
        let (trigger, action) =
            parse_procedure_content("when output too long → give concise summary");
        assert_eq!(trigger, "when output too long");
        assert_eq!(action, "give concise summary");

        let (trigger, action) = parse_procedure_content("error occurred -> retry once");
        assert_eq!(trigger, "error occurred");
        assert_eq!(action, "retry once");
    }

    // =====================================================================
    // Test 19: parse_procedure_content — when/if pattern
    // =====================================================================

    #[test]
    fn test_parse_procedure_content_when_pattern() {
        let (trigger, action) =
            parse_procedure_content("when user asks for summary, do reply concisely");
        assert_eq!(trigger, "user asks for summary");
        assert_eq!(action, "reply concisely");

        let (trigger, action) = parse_procedure_content("if network error, retry once");
        assert_eq!(trigger, "network error");
        assert_eq!(action, "retry once");
    }

    // =====================================================================
    // Test 20: parse_procedure_content — no separator fallback
    // =====================================================================

    #[test]
    fn test_parse_procedure_content_no_separator() {
        let (trigger, action) = parse_procedure_content("user prefers concise output");
        assert_eq!(trigger, "user prefers concise output");
        assert_eq!(action, "user prefers concise output");
    }

    // =====================================================================
    // Test 21: Procedure dedup — identical embedding → boost, not create
    // =====================================================================

    #[test]
    fn test_process_procedure_dedup_boost() {
        let store = test_store();

        // First procedure — confidence 0.85.
        let input1 = MemoryStoreInput {
            content: "when output too long → give concise summary".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.85),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };
        let result1 = store.process_memory_store(&input1).unwrap();
        assert!(result1.is_some());
        let id1 = result1.unwrap().node_id;

        // Second procedure with same embedding → should boost, not create.
        let input2 = MemoryStoreInput {
            content: "when response is lengthy, summarize briefly".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.85),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)), // identical
        };
        let result2 = store.process_memory_store(&input2).unwrap();
        assert!(result2.is_some());
        let id2 = result2.unwrap().node_id;

        // Same node ID — boosted, not created.
        assert_eq!(id1, id2, "duplicate procedure should return existing ID");

        // Confidence should have been boosted.
        let node = store.get_procedural(id1).unwrap().unwrap();
        assert!(
            node.confidence > 0.85,
            "confidence should be boosted from 0.85, got {}",
            node.confidence
        );

        // Only one procedural node should exist.
        let graph = store.db.graph_store();
        let count = graph.nodes_by_label(labels::PROCEDURAL).len();
        assert_eq!(count, 1, "should have exactly one procedural node");
    }

    // =====================================================================
    // Test 22: Procedure dedup — different embedding → create new
    // =====================================================================

    #[test]
    fn test_process_procedure_no_dedup_different_embedding() {
        let store = test_store();

        let input1 = MemoryStoreInput {
            content: "when output too long → give concise summary".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.85),
            source_episode_id: None,
            embedding: Some(const_emb(1.0)),
        };
        store.process_memory_store(&input1).unwrap();

        // Different embedding (flip 40 → cos ≈ 0.792, below 0.90 threshold).
        let input2 = MemoryStoreInput {
            content: "when network error occurs → retry once".to_string(),
            sub_type: KnowledgeSubType::Procedure,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.85),
            source_episode_id: None,
            embedding: Some(flipped_emb(40)),
        };
        let result2 = store.process_memory_store(&input2).unwrap();
        assert!(result2.is_some());

        // Two procedural nodes should exist.
        let graph = store.db.graph_store();
        let count = graph.nodes_by_label(labels::PROCEDURAL).len();
        assert_eq!(count, 2, "different procedures should both be stored");
    }
}
