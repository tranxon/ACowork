//! Offline consolidation — background upgrade of Pending knowledge nodes.
//!
//! Phase 2 implements a simple age-and-evidence upgrade strategy.
//! Phase 3 adds full LLM-based re-evaluation and generalization.
//!
//! ADR-057 P0 (C7): the previous Step-2 *LLM triple re-extraction from
//! unconsolidated episodes* has been deleted. New episodes are routed
//! through [`crate::consolidation::distill::ingest_distilled_triples`]
//! during compaction, so re-extracting on a background schedule would
//! duplicate cost and create race conditions with the synchronous landing
//! pipeline. The remaining steps (Pending upgrade/downgrade, generalization,
//! episodic forgetting) continue to run as before.
//!
//! ADR-068 M8: history compression and Relationship auto-generation have
//! been removed from this module. History nodes are event-triggered
//! (EpisodicDistiller), and 30-day Relationship nodes are produced by
//! [`crate::consolidation::distiller::EpisodicDistiller::promote_autobio_relationship`]
//! — this module only exposes the raw span stats via
//! `GrafeoStore::collaboration_span`.

use std::sync::Arc;

use acowork_memory::types::CollaborationSpan;
use chrono::{DateTime, TimeDelta, Utc};
use grafeo_common::types::Value;

use crate::consolidation::generalization::GeneralizationConfig;
use crate::consolidation::triple_extraction::TripleExtractorLlm;
use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{KnowledgeNode, NodeStatus, labels};
use acowork_memory::types::EpisodicDecayConfig;

// ---------------------------------------------------------------------------
// Configuration & Result (re-exported from acowork-memory)
// ---------------------------------------------------------------------------

pub use acowork_memory::consolidation::{OfflineConsolidationConfig, OfflineConsolidationResult};

/// Result of LLM conflict resolution during offline consolidation.
///
/// ADR-057 C7: this struct is **kept** for API stability — `run_offline_consolidation`
/// no longer populates the fields (Step-2 LLM extraction was deleted). New
/// conflict resolution happens at landing time via
/// [`crate::consolidation::distill::ingest_distilled_triples`].
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictResolutionResult {
    /// Total conflicts resolved.
    pub resolved: usize,
    /// Conflicts classified as Evolution (old -> Dormant).
    pub evolution: usize,
    /// Conflicts classified as Correction (old -> Dormant).
    pub correction: usize,
    /// Conflicts classified as Ambiguous (both kept).
    pub ambiguous: usize,
}

// GrafeoStore methods
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Run offline consolidation on pending nodes, including the lifecycle
    /// pipeline.
    ///
    /// Pipeline steps:
    /// 1. Standard offline consolidation (upgrade/downgrade Pending nodes)
    /// 2. ~~Triple extraction from unconsolidated episodes~~ (DELETED in ADR-057 C7)
    /// 3. ~~Conflict resolution via LLM arbitration~~ (DELETED in ADR-057 C7)
    /// 4. ~~Experience generalization to extract ProceduralNodes~~ (RETIRED in
    ///    ADR-068 revision — rule-based text counting over assistant content
    ///    produced unverifiable ProceduralNodes with no evidence/audit; the
    ///    EpisodicDistiller's `promote_procedures` is the sole producer)
    /// 5. ~~Compress History nodes~~ (DELETED in ADR-068 — episodic retention)
    /// 6. ~~Auto-generate Relationship nodes~~ (MOVED to EpisodicDistiller
    ///    `promote_autobio_relationship`, ADR-068 M8)
    ///    (~~7. Limitation generation~~ DELETED — false positives, see review doc)
    ///
    /// Note: this method does not use `tracing` — the grafeo crate
    /// intentionally avoids that dependency. The caller (runtime)
    /// logs the returned `OfflineConsolidationResult` fields instead.
    ///
    /// Deprecated (ADR-068 revision): the `llm` / `embedding_fn` /
    /// `gen_config` parameters and the `_with_generalization` suffix are
    /// legacy. The method name and signature are kept for API stability;
    /// callers should migrate to [`GrafeoStore::run_offline_consolidation`].
    #[allow(clippy::type_complexity)]
    pub async fn run_offline_consolidation_with_generalization(
        &self,
        config: &OfflineConsolidationConfig,
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: Option<Arc<dyn Fn(&str) -> Vec<f32> + Send + Sync>>,
        gen_config: Option<&GeneralizationConfig>,
    ) -> Result<OfflineConsolidationResult> {
        // Step 1: Standard offline consolidation (upgrade/downgrade Pending nodes).
        let mut result = self.run_offline_consolidation(config)?;

        // ADR-057 C7: Step 2 (LLM triple extraction from unconsolidated
        // episodes) and Step 3 (LLM conflict arbitration) are deleted.
        // New episodes land synchronously through `ingest_distilled_triples`,
        // so there is no batch of unconsolidated episodes to re-extract from
        // here. The `triples_extracted`, `conflicts_resolved`,
        // `conflicts_evolution`, `conflicts_correction`, `conflicts_ambiguous`
        // fields in `OfflineConsolidationResult` remain at zero — they are
        // kept for serialization compatibility and may be repopulated by
        // future ad-hoc reprocessing jobs.

        // Step 4 (ADR-068 revision): Experience generalization is RETIRED —
        // this method no longer runs it. Procedural promotion happens
        // exclusively in the EpisodicDistiller (`promote_procedures`) with
        // server-side LLM extraction, embedding clustering and a full audit
        // trail. The legacy parameters are unused; they remain only to keep
        // the trait/API stable while callers migrate.
        let _ = (llm, embedding_fn, gen_config);

        // Step 5 (ADR-068 removed): History-node compression is gone.
        // Episodic retention (Step 7) handles space reclamation via
        // mark_consolidated + cleanup; `history_compressed` stays at 0.

        // Step 6 (ADR-068 M8): Relationship auto-generation moved out of the
        // offline path — it now runs inside the EpisodicDistiller
        // (`promote_autobio_relationship`) so the Relationship category has a
        // single producer with a full audit trail. The raw span stats the
        // distiller needs are exposed via `GrafeoStore::collaboration_span`.

        // Step 7: Run episodic forgetting via the new time-decay engine
        // (ADR-057 §5.3 redesign). The old 7/14-day binary rule
        // (`run_episodic_cleanup`) is deleted — episodic nodes now decay on
        // a single half-life curve (`run_episodic_decay_scan`), the same
        // engine the runtime consolidation loop schedules. Default config:
        // this legacy path is a manual/admin trigger, so it runs the decay
        // scan with the system defaults (the runtime applies the per-agent
        // `agent_config.json` overrides).
        let decay_config = EpisodicDecayConfig::default();
        let decay_result = self.run_episodic_decay_scan(&decay_config)?;
        result.episodic_cleaned = (decay_result.to_dormant + decay_result.purged) as usize;

        Ok(result)
    }

    /// Run offline consolidation on pending nodes.
    ///
    /// Phase 2 strategy: upgrade Pending nodes to Active if they are older
    /// than `min_pending_age_hours` and have a confidence >= 0.7 (basic
    /// evidence threshold). Nodes with very low confidence (< 0.3) are
    /// downgraded to Dormant.
    ///
    /// Phase 3: Full LLM-based re-evaluation is available via
    /// `run_offline_consolidation_with_generalization`.
    pub fn run_offline_consolidation(
        &self,
        config: &OfflineConsolidationConfig,
    ) -> Result<OfflineConsolidationResult> {
        let pending_nodes =
            self.get_pending_for_consolidation(config.min_pending_age_hours, config.batch_size)?;

        let mut result = OfflineConsolidationResult::default();

        for mut node in pending_nodes {
            // Confidence gates from ADR-062 ConsolidationQuality
            // (dormant_confidence / pending_upgrade_threshold).
            let quality = self.quality();
            if node.confidence < quality.consolidation.dormant_confidence {
                // Very low confidence → mark Dormant.
                node.status = NodeStatus::Dormant;
                node.updated_at = Utc::now();
                self.update_knowledge(&node)?;
                result.marked_dormant += 1;
            } else if node.confidence >= quality.consolidation.pending_upgrade_threshold {
                // Reasonable confidence and old enough → upgrade to Active.
                node.status = NodeStatus::Active;
                node.updated_at = Utc::now();
                self.update_knowledge(&node)?;
                result.upgraded += 1;
            } else {
                // Between 0.3 and 0.7 — keep Pending, wait for more evidence.
                result.kept_pending += 1;
            }
        }

        Ok(result)
    }

    /// Get pending knowledge nodes that are old enough for offline processing.
    ///
    /// Returns up to `limit` nodes whose `created_at` is at least
    /// `min_age_hours` hours ago and whose status is `Pending`.
    pub fn get_pending_for_consolidation(
        &self,
        min_age_hours: u64,
        limit: usize,
    ) -> Result<Vec<KnowledgeNode>> {
        let cutoff = Utc::now() - TimeDelta::hours(min_age_hours as i64);
        let cutoff_us = cutoff.timestamp_micros();

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::KNOWLEDGE);

        let mut pending = Vec::new();

        for id in node_ids {
            if pending.len() >= limit {
                break;
            }

            if let Some(n) = self.db.get_node(id) {
                // Check status == Pending.
                let status_match = n
                    .get_property("status")
                    .and_then(Value::as_str)
                    .map(|s| s == "Pending")
                    .unwrap_or(false);

                if !status_match {
                    continue;
                }

                // Check created_at is old enough.
                let is_old_enough = n
                    .get_property("created_at")
                    .and_then(|v| v.as_timestamp())
                    .map(|ts| ts.as_micros() <= cutoff_us)
                    .unwrap_or(false);

                if !is_old_enough {
                    continue;
                }

                // Reconstruct the full KnowledgeNode.
                let props: Vec<(String, Value)> = n
                    .properties_as_btree()
                    .into_iter()
                    .map(|(k, v)| (k.as_str().to_string(), v))
                    .collect();
                let kn = KnowledgeNode::from_properties(id, &props)?;
                pending.push(kn);
            }
        }

        Ok(pending)
    }


    // ADR-057 C7: `get_unconsolidated_episode_contents` (deleted) and
    // `resolve_conflicts_with_llm` (deleted) were the supporting methods for
    // the Step-2 LLM extraction that has been removed. The conflict
    // classification helpers remain reachable from the
    // `consolidation::conflict_llm` module for unit tests, but no production
    // code calls them.

    /// Collaboration span across all episodes (ADR-068 M8).
    ///
    /// Returns the earliest stored episode timestamp and the total episode
    /// count, or `None` when no episodes exist. This replaced the offline
    /// `auto_generate_relationship_nodes` step: the 30-day Relationship
    /// decision now lives in the EpisodicDistiller, which consumes these
    /// stats through the `MemoryProvider::collaboration_span` trait method.
    pub fn collaboration_span(&self) -> Result<Option<CollaborationSpan>> {
        // Find the earliest episodic node + total count.
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut earliest_time: Option<DateTime<Utc>> = None;
        let mut episode_count: u64 = 0;

        for id in node_ids {
            episode_count += 1;
            if let Some(n) = self.db.get_node(id)
                && let Some(ts) = n.get_property("created_at").and_then(Value::as_timestamp)
                && let Some(dt) = DateTime::from_timestamp_micros(ts.as_micros())
            {
                match earliest_time {
                    None => earliest_time = Some(dt),
                    Some(earliest) if dt < earliest => earliest_time = Some(dt),
                    _ => {}
                }
            }
        }

        Ok(earliest_time.map(|earliest_episode_at| CollaborationSpan {
            earliest_episode_at,
            episode_count,
        }))
    }

    // Auto-generate Limitation autobiographical nodes — DELETED.
    //
    // This duplicated the runtime `MemoryManager::run_self_evaluation` logic
    // (same success-rate threshold, same key) but produced false-positive
    // Limitation nodes: `success_count` is never incremented anywhere in the
    // codebase, so any skill with >= 5 failures was flagged at 0% success.
    // Removed along with the runtime channel (memory write-entrypoint
    // decisions, see docs/memory-write-entrypoints.md).

    // ADR-057 C7: `apply_knowledge_updates` (deleted). The Step-2 LLM
    // triple-extraction pipeline it served has been removed; new triples are
    // landed through `ingest_distilled_triples` which performs its own
    // status dispatch and dedup. The old routine is intentionally absent.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DEFAULT_EMBEDDING_DIM, KnowledgeSubType, PrivacyLevel};

    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    // =====================================================================
    // Test: Offline consolidation upgrades old pending nodes
    // =====================================================================

    #[test]
    fn test_offline_consolidation_upgrade_pending_to_active() {
        let store = test_store();

        // Create a Pending node that is old enough.
        let old_time = Utc::now() - TimeDelta::hours(2);
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "coffee".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.75,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&node).unwrap();

        let config = OfflineConsolidationConfig {
            batch_size: 50,
            min_pending_age_hours: 1,
        };

        let result = store.run_offline_consolidation(&config).unwrap();
        assert_eq!(result.upgraded, 1);
        assert_eq!(result.kept_pending, 0);
        assert_eq!(result.marked_dormant, 0);
    }

    // =====================================================================
    // Test: Low confidence pending node → Dormant
    // =====================================================================

    #[test]
    fn test_offline_consolidation_low_confidence_to_dormant() {
        let store = test_store();

        let old_time = Utc::now() - TimeDelta::hours(2);
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "something".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.2,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&node).unwrap();

        let config = OfflineConsolidationConfig::default();
        let result = store.run_offline_consolidation(&config).unwrap();
        assert_eq!(result.marked_dormant, 1);
    }

    // =====================================================================
    // Test: Recent pending node → not processed
    // =====================================================================

    #[test]
    fn test_offline_consolidation_recent_pending_kept() {
        let store = test_store();

        // A Pending node that is too new.
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "tea".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.75,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: Utc::now(), // just created
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&node).unwrap();

        let config = OfflineConsolidationConfig {
            batch_size: 50,
            min_pending_age_hours: 1,
        };

        let result = store.run_offline_consolidation(&config).unwrap();
        assert_eq!(result.upgraded, 0);
        assert_eq!(result.kept_pending, 0); // not even returned by get_pending
    }

    // =====================================================================
    // Test: Active nodes are not affected
    // =====================================================================

    #[test]
    fn test_offline_consolidation_active_nodes_untouched() {
        let store = test_store();

        let old_time = Utc::now() - TimeDelta::hours(2);
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "chocolate".to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.75,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        let id = store.store_knowledge(&node).unwrap();

        let config = OfflineConsolidationConfig::default();
        let result = store.run_offline_consolidation(&config).unwrap();
        assert_eq!(result.upgraded, 0);

        // Active node should remain Active.
        let fetched = store.get_knowledge(id).unwrap().unwrap();
        assert_eq!(fetched.status, NodeStatus::Active);
    }

    // =====================================================================
    // Test: Default config values
    // =====================================================================

    #[test]
    fn test_offline_consolidation_default_config() {
        let config = OfflineConsolidationConfig::default();
        assert_eq!(config.batch_size, 50);
        assert_eq!(config.min_pending_age_hours, 1);
    }

    // =====================================================================
    // Test: get_pending_for_consolidation respects limit
    // =====================================================================

    #[test]
    fn test_get_pending_for_consolidation_respects_limit() {
        let store = test_store();

        let old_time = Utc::now() - TimeDelta::hours(2);
        for i in 0..5 {
            let node = KnowledgeNode {
                id: None,
                subject: "user".to_string(),
                predicate: format!("item_{i}"),
                object: "value".to_string(),
                sub_type: KnowledgeSubType::Fact,
                confidence: 0.6,
                source_episode_id: None,
                source_episode_ids: Vec::new(),
                promotion_metadata: None,
                embedding: Some(test_embedding()),
                status: NodeStatus::Pending,
                created_at: old_time,
                updated_at: old_time,
                metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
            };
            store.store_knowledge(&node).unwrap();
        }

        let pending = store.get_pending_for_consolidation(1, 3).unwrap();
        assert_eq!(pending.len(), 3, "should respect limit of 3");
    }

    // =====================================================================
    // Test: Medium confidence (0.3-0.7) kept as pending
    // =====================================================================

    #[test]
    fn test_offline_consolidation_medium_confidence_kept_pending() {
        let store = test_store();

        let old_time = Utc::now() - TimeDelta::hours(2);
        let node = KnowledgeNode {
            id: None,
            subject: "user".to_string(),
            predicate: "likes".to_string(),
            object: "maybe".to_string(),
            sub_type: KnowledgeSubType::Preference,
            confidence: 0.5,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance: 0.5,
        };
        store.store_knowledge(&node).unwrap();

        let config = OfflineConsolidationConfig {
            batch_size: 50,
            min_pending_age_hours: 1,
        };

        let result = store.run_offline_consolidation(&config).unwrap();
        assert_eq!(result.kept_pending, 1);
        assert_eq!(result.upgraded, 0);
        assert_eq!(result.marked_dormant, 0);
    }


    // =====================================================================
    // Note: the legacy `run_episodic_cleanup` tests (7/14-day binary rules)
    // were deleted with the function — episodic forgetting now uses the
    // single half-life curve in `run_episodic_decay_scan` (ADR-057 §5.3
    // redesign, covered by `forgetting::episodic_decay` tests).
}
