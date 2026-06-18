//! Offline consolidation — background upgrade of Pending knowledge nodes.
//!
//! Phase 2 implements a simple age-and-evidence upgrade strategy.
//! Phase 3 adds full LLM-based re-evaluation and generalization.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use grafeo_common::types::Value;

use crate::consolidation::conflict_llm::{LlmConflictType, classify_conflict};
use crate::consolidation::generalization::GeneralizationConfig;
use crate::consolidation::triple_extraction::{ExtractedTriple, TripleExtractorLlm};
use crate::error::Result;
use crate::grafeo::GrafeoStore;
use crate::types::{AutobioCategory, AutobiographicalNode, KnowledgeNode, NodeStatus, labels};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Offline consolidation configuration.
#[derive(Debug, Clone)]
pub struct OfflineConsolidationConfig {
    /// Maximum number of pending nodes to process per batch.
    /// Default: 50.
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible for
    /// offline processing. Default: 1.
    pub min_pending_age_hours: u64,
}

impl Default for OfflineConsolidationConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            min_pending_age_hours: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Result of LLM conflict resolution during offline consolidation.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictResolutionResult {
    /// Total conflicts resolved.
    pub resolved: usize,
    /// Conflicts classified as Evolution (old → Dormant).
    pub evolution: usize,
    /// Conflicts classified as Correction (old → Dormant).
    pub correction: usize,
    /// Conflicts classified as Ambiguous (both kept).
    pub ambiguous: usize,
}

/// Result of an offline consolidation run.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct OfflineConsolidationResult {
    /// Number of nodes upgraded from Pending → Active.
    pub upgraded: usize,
    /// Number of nodes kept as Pending (not old enough or not enough evidence).
    pub kept_pending: usize,
    /// Number of nodes marked Dormant (low confidence after re-evaluation).
    pub marked_dormant: usize,
    /// Number of new ProceduralNodes created by generalization.
    pub procedural_created: usize,
    /// Number of existing ProceduralNodes boosted by generalization.
    pub procedural_boosted: usize,
    /// Number of History nodes compressed into summaries.
    pub history_compressed: usize,
    /// Number of triples extracted from unconsolidated episodes.
    pub triples_extracted: usize,
    /// Number of conflicts resolved by LLM arbitration.
    pub conflicts_resolved: usize,
    /// Number of conflicts classified as Evolution (old → Dormant, new → Active).
    pub conflicts_evolution: usize,
    /// Number of conflicts classified as Correction (old → Dormant, new → Active).
    pub conflicts_correction: usize,
    /// Number of conflicts classified as Ambiguous (both kept, user confirmation needed).
    pub conflicts_ambiguous: usize,
    /// Number of episodic nodes cleaned up (transitioned to Dormant by §2 rules).
    pub episodic_cleaned: usize,
}

// ---------------------------------------------------------------------------
// GrafeoStore methods
// ---------------------------------------------------------------------------

impl GrafeoStore {
    /// Run offline consolidation on pending nodes, including full Phase 3 pipeline.
    ///
    /// Pipeline steps:
    /// 1. Standard offline consolidation (upgrade/downgrade Pending nodes)
    /// 2. Triple extraction from unconsolidated episodes (if LLM available)
    /// 3. Conflict resolution via LLM arbitration (if LLM available)
    /// 4. Experience generalization to extract ProceduralNodes
    /// 5. Compress History nodes if too many
    /// 6. Auto-generate Relationship nodes for long-term users
    /// 7. Auto-generate Limitation nodes from low success-rate skills
    ///
    /// Note: this method does not use `tracing` — the grafeo crate
    /// intentionally avoids that dependency. The caller (runtime)
    /// logs the returned `OfflineConsolidationResult` fields instead.
    #[allow(clippy::type_complexity)]
    pub async fn run_offline_consolidation_with_generalization(
        &self,
        config: &OfflineConsolidationConfig,
        llm: Option<&dyn TripleExtractorLlm>,
        embedding_fn: Option<Arc<dyn Fn(&str) -> Vec<f32> + Send + Sync>>,
        gen_config: Option<&GeneralizationConfig>,
    ) -> Result<OfflineConsolidationResult> {
        // Step 1: Standard offline consolidation (upgrade/downgrade Pending nodes)
        let mut result = self.run_offline_consolidation(config)?;

        // Step 2: Triple extraction from unconsolidated episodes (if LLM available).
        if let Some(llm_ref) = llm
            && let Some(ref emb_fn) = embedding_fn {
                let episode_contents =
                    self.get_unconsolidated_episode_contents(config.batch_size)?;
                if !episode_contents.is_empty() {
                    match self
                        .extract_triples(&episode_contents, llm_ref, emb_fn)
                        .await
                    {
                        Ok(extraction_result) => {
                            result.triples_extracted = extraction_result.triples.len();

                            // T4.5: Apply knowledge updates for triples where
                            // (subject, predicate) matches but object differs.
                            // This marks the old node as Dormant before the new
                            // triple is stored.
                            if !extraction_result.triples.is_empty() {
                                let updated =
                                    self.apply_knowledge_updates(&extraction_result.triples)?;
                                // Track: updated nodes are not "new" triples,
                                // they replace existing ones.
                                let _ = updated;
                            }

                            // Step 3: Resolve conflicts between extracted and existing knowledge.
                            if extraction_result.deduplicated > 0 {
                                let conflict_result =
                                    self.resolve_conflicts_with_llm(llm_ref).await?;
                                result.conflicts_resolved = conflict_result.resolved;
                                result.conflicts_evolution = conflict_result.evolution;
                                result.conflicts_correction = conflict_result.correction;
                                result.conflicts_ambiguous = conflict_result.ambiguous;
                            }
                        }
                        Err(_) => {
                            // Triple extraction failed — continue with remaining steps.
                            // Error is captured in result.triples_extracted remaining 0.
                        }
                    }
                }
            }

        // Step 4: Experience generalization (if embedding function provided)
        if let Some(ref emb_fn) = embedding_fn {
            let gen_config = gen_config.cloned().unwrap_or_default();
            let gen_result = self.run_generalization(llm, emb_fn, &gen_config).await?;
            result.procedural_created = gen_result.nodes_created;
            result.procedural_boosted = gen_result.nodes_boosted;
        }

        // Step 5: Compress History nodes if there are too many (> 10).
        result.history_compressed = self.compress_history_nodes(10)?;

        // Step 6: Auto-generate Relationship nodes for long-term users.
        // Per design §3.3: collaboration > 30 days → Relationship node.
        let _ = self.auto_generate_relationship_nodes()?;

        // Step 7: Auto-generate Limitation nodes from low success-rate skills.
        // Per design §3.3: skill success rate < 60% with >= 5 observations → Limitation node.
        let _ = self.auto_generate_limitation_nodes()?;

        // Step 8: Run episodic forgetting scan.
        // Per design §2: consolidated episodes > 7 days old are candidates for
        // decay → Dormant. Unconsolidated > 14 days with importance < 0.3 → Dormant.
        // Unconsolidated > 14 days with importance >= 0.3 → keep and trigger offline consolidation.
        result.episodic_cleaned = self.run_episodic_cleanup()?;

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
            if node.confidence < 0.3 {
                // Very low confidence → mark Dormant.
                node.status = NodeStatus::Dormant;
                node.updated_at = Utc::now();
                self.update_knowledge(&node)?;
                result.marked_dormant += 1;
            } else if node.confidence >= 0.7 {
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

    /// Compress History autobiographical nodes when they exceed a threshold.
    ///
    /// When there are more than `max_history_nodes` (default 10) History
    /// nodes, this method groups them by month and creates summary nodes.
    /// The original History nodes are marked Dormant (not deleted).
    ///
    /// This is a rule-based compression (no LLM). Phase 3 will add
    /// LLM-based summarization for richer compression.
    ///
    /// Returns the number of History nodes compressed (marked Dormant).
    pub fn compress_history_nodes(&self, max_history_nodes: usize) -> Result<usize> {
        // Find all History autobiographical nodes.
        let history_nodes = self.find_autobiographical_by_category(AutobioCategory::History)?;

        if history_nodes.len() <= max_history_nodes {
            return Ok(0); // Nothing to compress
        }

        // Group by month (YYYY-MM format).
        let mut monthly: std::collections::BTreeMap<String, Vec<AutobiographicalNode>> =
            std::collections::BTreeMap::new();

        for node in &history_nodes {
            let month_key = node.created_at.format("%Y-%m").to_string();
            monthly.entry(month_key).or_default().push(node.clone());
        }

        let mut compressed = 0usize;

        for (month, nodes) in monthly {
            if nodes.len() <= 1 {
                // Single node in a month — keep it Active.
                continue;
            }

            // Create a summary node for this month.
            let summary_value = nodes
                .iter()
                .map(|n| n.value.as_str())
                .collect::<Vec<_>>()
                .join("；");

            // Truncate to 200 chars (not bytes) to avoid splitting multi-byte UTF-8.
            let truncated = if summary_value.chars().count() > 200 {
                let s: String = summary_value.chars().take(200).collect();
                format!("{}…", s)
            } else {
                summary_value
            };

            let summary_key = format!("history_summary_{}", month);
            let summary_node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::History,
                key: summary_key,
                value: truncated,
                confidence: 0.9,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: nodes[0].created_at,
                updated_at: Utc::now(),
                metadata: std::collections::HashMap::new(),
            };

            self.store_autobiographical(&summary_node)?;

            // Mark original nodes as Dormant.
            for mut node in nodes {
                if node.id.is_some() {
                    node.status = NodeStatus::Dormant;
                    node.updated_at = Utc::now();
                    self.update_autobiographical(&node)?;
                    compressed += 1;
                }
            }
        }

        Ok(compressed)
    }

    /// Get episode content from unconsolidated episodic nodes.
    ///
    /// Returns (episode_id, content) pairs for episodes that have not yet
    /// been consolidated (consolidated == false).
    pub fn get_unconsolidated_episode_contents(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut episodes = Vec::new();

        for id in node_ids {
            if episodes.len() >= limit {
                break;
            }

            if let Some(n) = self.db.get_node(id) {
                // Check consolidated flag — skip already-consolidated.
                let is_consolidated = n
                    .get_property("consolidated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                if is_consolidated {
                    continue;
                }

                // Extract content.
                let content = n
                    .get_property("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                if content.is_empty() {
                    continue;
                }

                let episode_id = format!("{}", id.as_u64());
                episodes.push((episode_id, content));
            }
        }

        Ok(episodes)
    }

    /// Resolve knowledge conflicts using LLM arbitration.
    ///
    /// Finds pairs of knowledge nodes with the same (subject, predicate)
    /// but different (object), then classifies each conflict as
    /// Evolution, Correction, or Ambiguous using LLM.
    async fn resolve_conflicts_with_llm(
        &self,
        llm: &dyn TripleExtractorLlm,
    ) -> Result<ConflictResolutionResult> {
        let active = self.get_all_active_knowledge()?;

        // Group by (subject, predicate) to find conflicts.
        let mut groups: std::collections::HashMap<(String, String), Vec<&KnowledgeNode>> =
            std::collections::HashMap::new();
        for node in &active {
            let key = (node.subject.to_lowercase(), node.predicate.to_lowercase());
            groups.entry(key).or_default().push(node);
        }

        let mut result = ConflictResolutionResult::default();

        // Process groups with 2+ nodes (potential conflicts).
        for nodes in groups.values() {
            if nodes.len() < 2 {
                continue;
            }

            // Compare each pair (only the first two to limit LLM calls).
            let old = &nodes[0];
            let new = &nodes[1];

            // Only resolve if objects differ.
            if old.object.eq_ignore_ascii_case(&new.object) {
                continue;
            }

            match classify_conflict(
                &old.subject,
                &old.predicate,
                &old.object,
                &new.subject,
                &new.predicate,
                &new.object,
                None, // No evidence context available in offline mode
                llm,
            )
            .await
            {
                Ok(classification) => {
                    result.resolved += 1;
                    match classification.conflict_type {
                        LlmConflictType::Evolution => {
                            // Old value is outdated — mark Dormant, new stays Active.
                            let mut old_node = (*old).clone();
                            old_node.status = NodeStatus::Dormant;
                            old_node.updated_at = Utc::now();
                            self.update_knowledge(&old_node)?;
                            result.evolution += 1;
                        }
                        LlmConflictType::Correction => {
                            // Old value was wrong — mark Dormant, new stays Active.
                            let mut old_node = (*old).clone();
                            old_node.status = NodeStatus::Dormant;
                            old_node.updated_at = Utc::now();
                            self.update_knowledge(&old_node)?;
                            result.correction += 1;
                        }
                        LlmConflictType::Ambiguous => {
                            // Both could be true — keep both, mark for user confirmation.
                            // The Ambiguous system handles user confirmation hints separately.
                            result.ambiguous += 1;
                        }
                    }
                }
                Err(_) => {
                    // LLM conflict classification failed — keep both values.
                    result.ambiguous += 1;
                }
            }
        }

        Ok(result)
    }

    /// Auto-generate Relationship autobiographical nodes.
    ///
    /// Per design §3.3: if the user has collaborated with the agent for
    /// more than 30 days (based on earliest episodic record), create a
    /// Relationship node. Idempotent — skips if one already exists.
    fn auto_generate_relationship_nodes(&self) -> Result<usize> {
        // Check idempotency — skip if Relationship nodes already exist.
        let existing = self.find_autobiographical_by_category(AutobioCategory::Relationship)?;
        if !existing.is_empty() {
            return Ok(0);
        }

        // Find the earliest episodic node.
        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<Utc>> = None;
        let mut episode_count: u32 = 0;

        for id in node_ids {
            if let Some(n) = self.db.get_node(id) {
                episode_count += 1;
                if let Some(ts) = n.get_property("created_at").and_then(Value::as_timestamp)
                    && let Some(dt) = chrono::DateTime::from_timestamp_micros(ts.as_micros()) {
                        match earliest_time {
                            None => earliest_time = Some(dt),
                            Some(earliest) if dt < earliest => earliest_time = Some(dt),
                            _ => {}
                        }
                    }
            }
        }

        let Some(earliest) = earliest_time else {
            return Ok(0);
        };

        let span_days = (Utc::now() - earliest).num_days();
        if span_days < 30 {
            return Ok(0);
        }

        let key = "collaboration_span".to_string();
        let value = format!("已合作 {} 天（{} 次对话记录）", span_days, episode_count);
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Relationship,
            key,
            value,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        self.store_autobiographical(&node)?;
        Ok(1)
    }

    /// Auto-generate Limitation autobiographical nodes.
    ///
    /// Per design §3.3: if a skill's ProceduralNodes indicate a success
    /// rate below 60% with >= 5 total observations, create a Limitation node.
    /// Idempotent — skips if one already exists for the skill.
    fn auto_generate_limitation_nodes(&self) -> Result<usize> {
        let procedures = self.get_all_procedural_nodes()?;

        // Aggregate success/fail counts by source_skill.
        let mut skill_stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for node in &procedures {
            if let Some(ref skill) = node.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += node.success_count;
                entry.1 += node.fail_count;
            }
        }

        let mut created = 0;
        for (skill, (success, fail)) in skill_stats {
            let total = success + fail;
            if total < 5 {
                continue; // Not enough observations.
            }
            let rate = success as f32 / total as f32;
            if rate >= 0.60 {
                continue; // Success rate is acceptable.
            }

            // Check idempotency — skip if Limitation node already exists.
            let key = format!("skill_{}", skill);
            if self.find_autobiographical_by_key(&key)?.is_some() {
                continue;
            }

            let rate_pct = (rate * 100.0) as u32;
            let value = format!(
                "{} 成功率仅 {}%（{} 次成功 / {} 次失败）",
                skill, rate_pct, success, fail
            );
            let node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::Limitation,
                key: key.clone(),
                value,
                confidence: 0.8,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: std::collections::HashMap::new(),
            };
            self.store_autobiographical(&node)?;
            created += 1;
        }

        Ok(created)
    }

    /// Run episodic memory cleanup based on design §2 forgetting rules.
    ///
    /// Unlike the general `run_decay_scan()` which uses the multiplicative
    /// decay formula on all labels, this method applies episodic-specific
    /// rules with explicit consolidated/unconsolidated distinction:
    ///
    /// - **Consolidated** episodes older than 7 days → Dormant
    ///   (knowledge already extracted; episode is redundant)
    /// - **Unconsolidated** episodes older than 14 days with importance < 0.3 → Dormant
    ///   (low-value and stale; unlikely to yield useful knowledge)
    /// - **Unconsolidated** episodes older than 14 days with importance >= 0.3 → keep Active
    ///   (high-value but missed consolidation; will be picked up next cycle)
    /// - **Unconsolidated** episodes older than 14 days with importance >= 0.3 → mark
    ///   `needs_consolidation = true` in metadata so the next consolidation cycle
    ///   prioritizes them.
    ///
    /// Returns the number of episodic nodes transitioned to Dormant.
    fn run_episodic_cleanup(&self) -> Result<usize> {
        let now = Utc::now();
        let mut transitioned = 0usize;

        let graph = self.db.graph_store();
        let node_ids = graph.nodes_by_label(labels::EPISODIC);

        for node_id in node_ids {
            if let Some(node) = self.db.get_node(node_id) {
                // Skip non-Active nodes.
                if let Some(Value::String(s)) = node.properties.get(&"status".into())
                    && s.as_str() != NodeStatus::Active.as_str() {
                        continue;
                    }

                // Read consolidated flag.
                let is_consolidated = node
                    .get_property("consolidated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                // Read importance (default 0.5 if not set).
                let importance = node
                    .get_property("importance")
                    .and_then(|v| v.as_float64())
                    .unwrap_or(0.5) as f32;

                // Compute age in days.
                let created_at = node
                    .get_property("created_at")
                    .and_then(|v| v.as_timestamp());
                let age_days = created_at
                    .and_then(|ts| {
                        DateTime::from_timestamp_micros(ts.as_micros())
                            .map(|dt| (now - dt).num_days())
                    })
                    .unwrap_or(0);

                // Apply design §2 rules.
                let should_dormant = if is_consolidated && age_days > 7 {
                    // Consolidated + > 7 days → Dormant.
                    true
                } else if !is_consolidated && age_days > 14 {
                    if importance < 0.3 {
                        // Unconsolidated + > 14 days + low importance → Dormant.
                        true
                    } else {
                        // Unconsolidated + > 14 days + high importance → keep Active,
                        // but mark for priority consolidation.
                        self.db.set_node_property(
                            node_id,
                            "needs_consolidation",
                            Value::from(true),
                        );
                        false
                    }
                } else {
                    false
                };

                if should_dormant {
                    self.transition_to_dormant(node_id)?;
                    transitioned += 1;
                }
            }
        }

        Ok(transitioned)
    }

    /// Enhanced Fact semantic deduplication.
    ///
    /// Beyond the existing `has_potential_conflict()` which checks
    /// (subject, predicate) exact match, this method also handles:
    /// - Same (subject, predicate) with different object → knowledge update
    ///   (replace old with new, mark old as Dormant) instead of creating duplicate
    ///
    /// This is called during triple extraction when dedup is detected.
    /// Returns the number of old nodes marked Dormant due to knowledge updates.
    pub fn apply_knowledge_updates(&self, new_triples: &[ExtractedTriple]) -> Result<usize> {
        let existing = self.get_all_active_knowledge()?;
        let mut updated = 0;

        for triple in new_triples {
            // Find nodes with matching subject+predicate but different object.
            for node in &existing {
                if node.subject.eq_ignore_ascii_case(&triple.subject)
                    && node.predicate.eq_ignore_ascii_case(&triple.predicate)
                    && !node.object.eq_ignore_ascii_case(&triple.object)
                {
                    // Knowledge update: mark old as Dormant, new will be stored
                    // by the caller (extract_triples) as Active.
                    let mut old = node.clone();
                    old.status = NodeStatus::Dormant;
                    old.updated_at = Utc::now();
                    self.update_knowledge(&old)?;
                    updated += 1;
                }
            }
        }

        Ok(updated)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AutobioCategory, AutobiographicalNode, DEFAULT_EMBEDDING_DIM, KnowledgeSubType,
    };

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
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
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
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
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
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: Utc::now(), // just created
            updated_at: Utc::now(),
            metadata: std::collections::HashMap::new(),
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
            embedding: Some(test_embedding()),
            status: NodeStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
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
                embedding: Some(test_embedding()),
                status: NodeStatus::Pending,
                created_at: old_time,
                updated_at: old_time,
                metadata: std::collections::HashMap::new(),
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
            embedding: Some(test_embedding()),
            status: NodeStatus::Pending,
            created_at: old_time,
            updated_at: old_time,
            metadata: std::collections::HashMap::new(),
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
    // Test: compress_history_nodes — no compression when ≤ 10 nodes
    // =====================================================================

    #[test]
    fn test_compress_history_nodes_no_compression_needed() {
        let store = test_store();

        // Create 5 History nodes (below threshold).
        for i in 0..5 {
            let node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::History,
                key: format!("milestone_{}", i),
                value: format!("Event {}", i),
                confidence: 0.9,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: std::collections::HashMap::new(),
            };
            store.store_autobiographical(&node).unwrap();
        }

        let compressed = store.compress_history_nodes(10).unwrap();
        assert_eq!(
            compressed, 0,
            "no compression should happen with ≤ 10 nodes"
        );
    }

    // =====================================================================
    // Test: compress_history_nodes — compresses when > 10 nodes
    // =====================================================================

    #[test]
    fn test_compress_history_nodes_compresses_over_threshold() {
        let store = test_store();

        // Create 12 History nodes in the same month.
        // Since there are > 10 and > 1 in a month, they should be compressed.
        let base_time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for i in 0..12 {
            let node = AutobiographicalNode {
                id: None,
                category: AutobioCategory::History,
                key: format!("milestone_{}", i),
                value: format!("Event {}", i),
                confidence: 0.9,
                source_episode_id: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: base_time + TimeDelta::days(i),
                updated_at: base_time + TimeDelta::days(i),
                metadata: std::collections::HashMap::new(),
            };
            store.store_autobiographical(&node).unwrap();
        }

        let compressed = store.compress_history_nodes(10).unwrap();
        assert!(compressed > 0, "should compress some History nodes");

        // Verify: some original nodes should be Dormant now.
        let history = store
            .find_autobiographical_by_category(AutobioCategory::History)
            .unwrap();
        let dormant_count = history
            .iter()
            .filter(|n| n.status == NodeStatus::Dormant)
            .count();
        assert!(dormant_count > 0, "some original nodes should be Dormant");

        // Verify: a summary node should exist.
        let summary = store
            .find_autobiographical_by_key("history_summary_2023-11")
            .unwrap();
        assert!(
            summary.is_some(),
            "a summary node should be created for the month"
        );
    }
}
