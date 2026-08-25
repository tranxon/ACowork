//! MemoryProvider trait implementation for GrafeoStore.
//!
//! This file implements the full `MemoryProvider` trait (ADR-051) for
//! `GrafeoStore`. It delegates to existing inherent methods and handles
//! type conversion between `acowork_memory` types (no `id` field) and
//! grafeo's internal types (with `id: Option<NodeId>`).

use std::sync::Arc;

use acowork_core::error::{AcoworkError, Result as AcoworkResult};
use acowork_memory::consolidation::{
    GeneralizationConfig, GeneralizationResult, MemoryStoreInput, MemoryStoreResult,
    OfflineConsolidationConfig, OfflineConsolidationResult, SchedulerConfig,
};
use acowork_memory::provider::{IngestResult, MemoryProvider};
use acowork_memory::{
    AutobioCategory, AutobiographicalNode, DecayConfig, DecayScanResult, Episode, KnowledgeNode,
    MemoryQuery, ProceduralNode, PurgeResult, SearchResult, StoreHealth, StoreStats,
};

use grafeo_common::types::NodeId;

use crate::grafeo::GrafeoStore;
use crate::types::{
    AutobiographicalNode as GrafeoAutobiographicalNode, Episode as GrafeoEpisode,
    KnowledgeNode as GrafeoKnowledgeNode, ProceduralNode as GrafeoProceduralNode,
};

// ============================================================================
// Type conversion helpers
// ============================================================================

fn grafeo_to_memory_procedural(node: GrafeoProceduralNode) -> ProceduralNode {
    ProceduralNode {
        id: node.id.map(|id| id.0),
        name: node.name,
        trigger_condition: node.trigger_condition,
        action_pattern: node.action_pattern,
        success_count: node.success_count,
        fail_count: node.fail_count,
        confidence: node.confidence,
        activation_count: node.activation_count,
        source_skill: node.source_skill,
        learned_from: node.learned_from,
        // Memory contract requires a vector; storage round-trips may carry
        // None (pre-ADR-057 nodes) which maps to an empty Vec.
        embedding: node.embedding.unwrap_or_default(),
        status: node.status,
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata,
    }
}

fn memory_to_grafeo_procedural(node: &ProceduralNode) -> GrafeoProceduralNode {
    GrafeoProceduralNode {
        id: node.id.map(NodeId),
        name: node.name.clone(),
        trigger_condition: node.trigger_condition.clone(),
        action_pattern: node.action_pattern.clone(),
        success_count: node.success_count,
        fail_count: node.fail_count,
        confidence: node.confidence,
        activation_count: node.activation_count,
        source_skill: node.source_skill.clone(),
        learned_from: node.learned_from.clone(),
        // Empty Vec = "no vector" on the memory side → store as absent.
        embedding: if node.embedding.is_empty() {
            None
        } else {
            Some(node.embedding.clone())
        },
        status: node.status.clone(),
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata.clone(),
    }
}

fn grafeo_to_memory_autobiographical(node: GrafeoAutobiographicalNode) -> AutobiographicalNode {
    AutobiographicalNode {
        id: node.id.map(|id| id.0),
        category: node.category,
        key: node.key,
        value: node.value,
        confidence: node.confidence,
        source_episode_id: node.source_episode_id.map(|id| id.0),
        embedding: node.embedding,
        status: node.status,
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata,
    }
}

fn memory_to_grafeo_autobiographical(node: &AutobiographicalNode) -> GrafeoAutobiographicalNode {
    GrafeoAutobiographicalNode {
        id: node.id.map(NodeId),
        category: node.category.clone(),
        key: node.key.clone(),
        value: node.value.clone(),
        confidence: node.confidence,
        source_episode_id: node.source_episode_id.map(NodeId::new),
        embedding: node.embedding.clone(),
        status: node.status.clone(),
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata.clone(),
    }
}

fn grafeo_to_memory_episode(ep: GrafeoEpisode) -> Episode {
    Episode {
        session_id: ep.session_id,
        turn_index: ep.turn_index,
        role: ep.role,
        content: ep.content,
        embedding: ep.embedding,
        timestamp: ep.timestamp,
        consolidated: ep.consolidated,
        metadata: ep.metadata,
        importance: ep.importance,
    }
}

fn memory_to_grafeo_episode(ep: &Episode) -> GrafeoEpisode {
    GrafeoEpisode {
        id: None,
        session_id: ep.session_id.clone(),
        turn_index: ep.turn_index,
        role: ep.role.clone(),
        content: ep.content.clone(),
        embedding: ep.embedding.clone(),
        timestamp: ep.timestamp,
        consolidated: ep.consolidated,
        metadata: ep.metadata.clone(),
        importance: ep.importance,
    }
}

fn memory_to_grafeo_knowledge(node: &KnowledgeNode) -> GrafeoKnowledgeNode {
    GrafeoKnowledgeNode {
        id: None,
        subject: node.subject.clone(),
        predicate: node.predicate.clone(),
        object: node.object.clone(),
        sub_type: node.sub_type.clone(),
        confidence: node.confidence,
        source_episode_id: None,
        embedding: node.embedding.clone(),
        status: node.status.clone(),
        created_at: node.created_at,
        updated_at: node.updated_at,
        metadata: node.metadata.clone(),
    }
}

fn err_to_acowork(e: crate::error::GrafeoError) -> AcoworkError {
    AcoworkError::Memory(e.to_string())
}

// ============================================================================
// MemoryProvider implementation
// ============================================================================

#[async_trait::async_trait]
impl MemoryProvider for GrafeoStore {
    // ── Episodic layer ───────────────────────────────────────────────────

    fn store_episode(&self, episode: &Episode) -> AcoworkResult<()> {
        let grafeo_ep = memory_to_grafeo_episode(episode);
        GrafeoStore::store_episode(self, &grafeo_ep)
            .map(|_| ())
            .map_err(err_to_acowork)
    }

    fn search_episodes(&self, query: &MemoryQuery) -> AcoworkResult<Vec<SearchResult>> {
        // Delegate to MemoryStore impl logic
        use acowork_memory::types::ResultSource;
        if let Some(ref embedding) = query.embedding {
            let episodes = self
                .search_episodes_by_embedding(embedding, query.limit)
                .map_err(err_to_acowork)?;
            Ok(episodes
                .into_iter()
                .map(|(ep, score)| SearchResult {
                    node_id: ep.id.map(|id| id.0).unwrap_or(0),
                    content: ep.content,
                    label: "Episodic".to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                })
                .collect())
        } else {
            let episodes = self
                .search_episodes_by_keyword(&query.query_text, query.limit)
                .map_err(err_to_acowork)?;
            Ok(episodes
                .into_iter()
                .map(|(ep, score)| SearchResult {
                    node_id: ep.id.map(|id| id.0).unwrap_or(0),
                    content: ep.content,
                    label: "Episodic".to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                })
                .collect())
        }
    }

    fn mark_consolidated(&self, ids: &[u64]) -> AcoworkResult<()> {
        for id in ids {
            self.mark_episode_consolidated(NodeId(*id))
                .map_err(err_to_acowork)?;
        }
        Ok(())
    }

    fn cleanup_episodes(&self, older_than: std::time::Duration) -> AcoworkResult<u64> {
        let retention_days = (older_than.as_secs() / 86400) as u32;
        let count = self
            .cleanup_old_episodes(retention_days)
            .map_err(err_to_acowork)?;
        Ok(count as u64)
    }

    fn get_episodes(&self, session_id: Option<&str>, limit: usize) -> AcoworkResult<Vec<Episode>> {
        let grafeo_eps = if let Some(sid) = session_id {
            self.search_episodes_by_session(sid, limit)
                .map_err(err_to_acowork)?
        } else {
            self.list_all_episodes(limit).map_err(err_to_acowork)?
        };
        Ok(grafeo_eps.into_iter().map(grafeo_to_memory_episode).collect())
    }

    // ── Semantic layer ───────────────────────────────────────────────────

    fn store_knowledge(&self, node: &KnowledgeNode) -> AcoworkResult<()> {
        let grafeo_node = memory_to_grafeo_knowledge(node);
        GrafeoStore::store_knowledge(self, &grafeo_node)
            .map(|_| ())
            .map_err(err_to_acowork)
    }

    fn store_procedural(&self, node: &ProceduralNode) -> AcoworkResult<()> {
        let grafeo_node = memory_to_grafeo_procedural(node);
        GrafeoStore::store_procedural(self, &grafeo_node)
            .map(|_| ())
            .map_err(err_to_acowork)
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> AcoworkResult<()> {
        let grafeo_node = memory_to_grafeo_autobiographical(node);
        GrafeoStore::store_autobiographical(self, &grafeo_node)
            .map(|_| ())
            .map_err(err_to_acowork)
    }

    // ── Unified retrieval ───────────────────────────────────────────────

    fn hybrid_search(&self, query: &MemoryQuery) -> AcoworkResult<Vec<SearchResult>> {
        use acowork_memory::types::ResultSource;
        let labels = ["Episodic", "Knowledge", "Procedural", "Autobiographical"];
        let mut all_results: Vec<SearchResult> = Vec::new();

        for label in &labels {
            if query.embedding.is_none() && query.query_text.is_empty() {
                continue;
            }
            let embedding = query.embedding.as_deref().unwrap_or(&[]);
            let search_results = if !embedding.is_empty() && !query.query_text.is_empty() {
                self.hybrid_search(
                    label,
                    "content",
                    "embedding",
                    &query.query_text,
                    embedding,
                    query.limit,
                )
                .map_err(err_to_acowork)?
            } else if !embedding.is_empty() {
                self.vector_search(label, embedding, query.limit, None)
                    .map_err(err_to_acowork)?
                    .into_iter()
                    .map(|(id, score)| (id, score as f64))
                    .collect()
            } else {
                self.text_search(label, &query.query_text, query.limit)
                    .map_err(err_to_acowork)?
            };

            for (node_id, score) in search_results {
                all_results.push(SearchResult {
                    node_id: node_id.0,
                    content: String::new(),
                    label: label.to_string(),
                    score,
                    source: ResultSource::DirectMatch,
                    context_tokens: 0,
                    source_context: None,
                });
            }
        }

        all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        if let Some(min_score) = query.min_score {
            all_results.retain(|r| r.score >= min_score as f64);
        }
        all_results.truncate(query.limit);
        Ok(all_results)
    }

    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> AcoworkResult<Vec<SearchResult>> {
        use acowork_memory::types::ResultSource;
        let seed_nodes: Vec<(NodeId, f64)> = seeds
            .iter()
            .map(|s| (NodeId(s.node_id), s.score))
            .collect();
        let config = crate::spreading::GraphExpandConfig {
            max_hops: hops as u32,
            ..Default::default()
        };
        let expanded = self
            .graph_expand(&seed_nodes, &config)
            .map_err(err_to_acowork)?;
        Ok(expanded
            .into_iter()
            .map(|node| SearchResult {
                node_id: node.node_id.0,
                content: String::new(),
                label: node.label,
                score: node.accumulated_score,
                source: ResultSource::GraphExpansion,
                context_tokens: 0,
                source_context: None,
            })
            .collect())
    }

    // ── Forgetting ───────────────────────────────────────────────────────

    fn run_decay_scan(&self, config: &DecayConfig) -> AcoworkResult<DecayScanResult> {
        let native_config = crate::forgetting::decay::DecayConfig {
            lambda: config.lambda as f64,
            access_boost: config.access_per_hit as f64,
            dormant_threshold: config.dormant_threshold,
        };
        let transitioned = self
            .run_decay_scan(&native_config)
            .map_err(err_to_acowork)?;
        Ok(DecayScanResult {
            to_dormant: transitioned as u64,
            reactivated: 0,
            purged: 0,
        })
    }

    fn reactivate_node(&self, node_id: u64) -> AcoworkResult<()> {
        GrafeoStore::reactivate_node(self, NodeId(node_id)).map_err(err_to_acowork)
    }

    fn purge_expired(&self, max_dormant_age: std::time::Duration) -> AcoworkResult<PurgeResult> {
        let max_days = (max_dormant_age.as_secs() / 86400) as u32;
        let purged_entries = self
            .purge_expired_dormant(max_days)
            .map_err(err_to_acowork)?;
        Ok(PurgeResult {
            purged_count: purged_entries.len() as u64,
            bytes_freed: 0,
        })
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    fn health_check(&self) -> AcoworkResult<StoreHealth> {
        Ok(StoreHealth {
            is_healthy: true,
            latency_ms: 0,
            error_count: 0,
            details: None,
        })
    }

    fn stats(&self) -> AcoworkResult<StoreStats> {
        let memory_stats = crate::stats::collect_stats(self).map_err(err_to_acowork)?;
        let episode_count = *memory_stats.label_counts.get("Episodic").unwrap_or(&0) as u64;
        let knowledge_count = *memory_stats.label_counts.get("Knowledge").unwrap_or(&0) as u64;
        let procedural_count = *memory_stats.label_counts.get("Procedural").unwrap_or(&0) as u64;
        let autobio_count = *memory_stats
            .label_counts
            .get("Autobiographical")
            .unwrap_or(&0) as u64;
        Ok(StoreStats {
            episode_count,
            node_count: knowledge_count + procedural_count + autobio_count,
            active_node_count: 0,
            dormant_node_count: memory_stats.dormant_count as u64,
            edge_count: 0,
            storage_size_bytes: 0,
            index_count: 0,
        })
    }

    fn close(&self) -> AcoworkResult<()> {
        GrafeoStore::close(self).map_err(err_to_acowork)
    }

    // ── Hybrid retrieval (extended) ──────────────────────────────────────

    fn hybrid_search_full(
        &self,
        label: &str,
        query_text: &str,
        embedding: &[f32],
        k: usize,
        text_weight: f64,
        vector_weight: f64,
        min_score: Option<f32>,
    ) -> AcoworkResult<Vec<(u64, f64)>> {
        let results = self
            .hybrid_search(
                label,
                "content",
                "embedding",
                query_text,
                embedding,
                k,
            )
            .map_err(err_to_acowork)?;
        // Apply weights and min_score filter
        Ok(results
            .into_iter()
            .filter(|(_, score)| min_score.is_none_or(|ms| *score as f32 >= ms))
            .map(|(id, score)| {
                let weighted = score * text_weight.max(vector_weight);
                (id.0, weighted)
            })
            .collect())
    }

    fn text_search_with_filter(
        &self,
        label: &str,
        field: &str,
        query_text: &str,
        k: usize,
        min_score: Option<f32>,
    ) -> AcoworkResult<Vec<(u64, f64)>> {
        let results = self
            .text_search_with_filter(label, field, query_text, k, min_score)
            .map_err(err_to_acowork)?;
        Ok(results.into_iter().map(|(id, score)| (id.0, score)).collect())
    }

    // ── memory_store tool entry ──────────────────────────────────────────

    fn process_memory_store(
        &self,
        input: &MemoryStoreInput,
    ) -> AcoworkResult<Option<MemoryStoreResult>> {
        // GrafeoStore::process_memory_store accepts the re-exported
        // acowork_memory::MemoryStoreInput type (with u64 source_episode_id).
        // Internal conversion to NodeId happens inside the method.
        GrafeoStore::process_memory_store(self, input).map_err(err_to_acowork)
    }

    // ── Ambiguous conflict confirmation ──────────────────────────────────

    fn should_trigger_confirmation(&self) -> AcoworkResult<bool> {
        GrafeoStore::should_trigger_confirmation(self).map_err(err_to_acowork)
    }

    fn generate_confirmation_hint(&self) -> AcoworkResult<Option<String>> {
        GrafeoStore::generate_confirmation_hint(self).map_err(err_to_acowork)
    }

    // ── Experience generalization (Path C) ───────────────────────────────

    async fn run_generalization(
        &self,
        _session_id: Option<&str>,
        embedding_fn: &Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>,
        config: &GeneralizationConfig,
    ) -> AcoworkResult<GeneralizationResult> {
        // Rebind as EmbeddingFn to satisfy grafeo method's type requirement.
        // The async_trait HRTB desugaring creates a subtle type mismatch;
        // cloning the Arc into a local variable with explicit type resolves it.
        let emb_fn: acowork_memory::consolidation::EmbeddingFn = embedding_fn.clone();
        GrafeoStore::run_generalization(self, None, &emb_fn, config)
            .await
            .map_err(err_to_acowork)
    }

    fn compress_history_nodes(&self, keep_recent: usize) -> AcoworkResult<usize> {
        GrafeoStore::compress_history_nodes(self, keep_recent).map_err(err_to_acowork)
    }

    // ── Node CRUD ────────────────────────────────────────────────────────

    fn get_all_procedural_nodes(&self) -> AcoworkResult<Vec<ProceduralNode>> {
        let nodes = GrafeoStore::get_all_procedural_nodes(self).map_err(err_to_acowork)?;
        Ok(nodes.into_iter().map(grafeo_to_memory_procedural).collect())
    }

    fn find_procedural_by_trigger(
        &self,
        trigger: &str,
        limit: usize,
    ) -> AcoworkResult<Vec<ProceduralNode>> {
        let nodes = self
            .find_procedural_by_trigger(trigger, limit)
            .map_err(err_to_acowork)?;
        Ok(nodes.into_iter().map(grafeo_to_memory_procedural).collect())
    }

    fn get_procedural(&self, node_id: u64) -> AcoworkResult<Option<ProceduralNode>> {
        let node = self
            .get_procedural(NodeId(node_id))
            .map_err(err_to_acowork)?;
        Ok(node.map(grafeo_to_memory_procedural))
    }

    fn update_procedural(&self, node: &ProceduralNode) -> AcoworkResult<()> {
        // Look up existing node to preserve id
        let grafeo_node = memory_to_grafeo_procedural(node);
        self.update_procedural(&grafeo_node).map_err(err_to_acowork)
    }

    fn find_autobiographical_by_key(
        &self,
        key: &str,
    ) -> AcoworkResult<Option<AutobiographicalNode>> {
        let node = self
            .find_autobiographical_by_key(key)
            .map_err(err_to_acowork)?;
        Ok(node.map(grafeo_to_memory_autobiographical))
    }

    fn find_autobiographical_by_category(
        &self,
        category: AutobioCategory,
    ) -> AcoworkResult<Vec<AutobiographicalNode>> {
        let nodes = self
            .find_autobiographical_by_category(category)
            .map_err(err_to_acowork)?;
        Ok(nodes
            .into_iter()
            .map(grafeo_to_memory_autobiographical)
            .collect())
    }

    fn update_autobiographical(&self, node: &AutobiographicalNode) -> AcoworkResult<()> {
        let grafeo_node = memory_to_grafeo_autobiographical(node);
        self.update_autobiographical(&grafeo_node)
            .map_err(err_to_acowork)
    }

    fn create_memory_edge(
        &self,
        from: u64,
        to: u64,
        edge_type: &str,
        properties: Vec<(&str, String)>,
    ) -> AcoworkResult<()> {
        let props: Vec<(String, grafeo_common::types::Value)> = properties
            .into_iter()
            .map(|(k, v)| (k.to_string(), grafeo_common::types::Value::from(v.as_str())))
            .collect();
        self.create_memory_edge(NodeId(from), NodeId(to), edge_type, props)
            .map(|_| ())
            .map_err(err_to_acowork)
    }

    // ── Retrieval pipeline support (ADR-051 C4) ──────────────────────────

    fn graph_expand_seeded(
        &self,
        seeds: &[(u64, f64)],
        hint_type: &str,
    ) -> AcoworkResult<Vec<(u64, f64, String)>> {
        let seed_nodes: Vec<(NodeId, f64)> = seeds
            .iter()
            .map(|(id, score)| (NodeId(*id), *score))
            .collect();
        let config = crate::spreading::config_from_hint(hint_type);
        let expanded = self
            .graph_expand(&seed_nodes, &config)
            .map_err(err_to_acowork)?;
        Ok(expanded
            .into_iter()
            .map(|node| (node.node_id.0, node.accumulated_score, node.label))
            .collect())
    }

    fn get_node_content(&self, node_id: u64) -> AcoworkResult<Option<String>> {
        let nid = NodeId(node_id);
        let Some(node) = self.db().get_node(nid) else {
            return Ok(None);
        };

        // Autobiographical nodes: include category for disambiguation.
        if let Some(category) = node.get_property("category").and_then(|v| v.as_str()) {
            let key = node
                .get_property("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let value = node
                .get_property("value")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !key.is_empty() && !value.is_empty() {
                return Ok(Some(format!("{category}: {key}: {value}")));
            }
            if !value.is_empty() {
                return Ok(Some(format!("{category}: {value}")));
            }
        }

        // Procedural nodes: format as behavioral guideline.
        let trigger = node
            .get_property("trigger_condition")
            .and_then(|v| v.as_str());
        let action = node
            .get_property("action_pattern")
            .and_then(|v| v.as_str());
        if let (Some(t), Some(a)) = (trigger, action) {
            return Ok(Some(format!("当 {t} 时，优先 {a}")));
        }

        // Try common content fields in priority order.
        if let Some(content) = node.get_property("content").and_then(|v| v.as_str()) {
            return Ok(Some(content.to_string()));
        }
        if let Some(value) = node.get_property("value").and_then(|v| v.as_str()) {
            return Ok(Some(value.to_string()));
        }

        // Knowledge nodes: combine subject + predicate + object.
        let subject = node.get_property("subject").and_then(|v| v.as_str());
        let predicate = node.get_property("predicate").and_then(|v| v.as_str());
        let object = node.get_property("object").and_then(|v| v.as_str());

        if let (Some(s), Some(p), Some(o)) = (subject, predicate, object) {
            return Ok(Some(format!("{s} {p} {o}")));
        }

        // Generic action_pattern fallback.
        if let Some(action) = node
            .get_property("action_pattern")
            .and_then(|v| v.as_str())
        {
            return Ok(Some(action.to_string()));
        }

        // Fallback: use any string property.
        for key in ["name", "key", "description"] {
            if let Some(v) = node.get_property(key).and_then(|v| v.as_str()) {
                return Ok(Some(v.to_string()));
            }
        }

        Ok(Some(String::new()))
    }

    fn get_node_session_id(&self, node_id: u64) -> AcoworkResult<Option<String>> {
        let nid = NodeId(node_id);
        match self.db().get_node(nid) {
            Some(node) => Ok(node
                .get_property("session_id")
                .map(|v| v.to_string().trim_matches('"').to_string())),
            None => Ok(None),
        }
    }

    fn apply_pagerank_boost(
        &self,
        scores: &mut [(u64, f64)],
        weight: f64,
    ) -> AcoworkResult<()> {
        let mut scored: Vec<(NodeId, f64)> = scores
            .iter()
            .map(|(id, score)| (NodeId(*id), *score))
            .collect();
        GrafeoStore::apply_pagerank_boost(self, &mut scored, weight).map_err(err_to_acowork)?;
        for (i, (_, boosted)) in scored.into_iter().enumerate() {
            scores[i].1 = boosted;
        }
        Ok(())
    }

    // ── Consolidation lifecycle ──────────────────────────────────────────
    //
    // P1 note: The ConsolidationScheduler currently lives in the Runtime
    // (AgentCore.consolidation_scheduler). These trait methods provide the
    // interface for future internalization. For now, start/stop/notify are
    // thin wrappers that store config. The actual scheduling is still
    // performed by the Runtime's background task (consolidation_bg.rs).

    fn start_consolidation(&self, _config: &SchedulerConfig) -> AcoworkResult<()> {
        // P1: Config is accepted but scheduling is still managed by the Runtime.
        // P3 will internalize the scheduler fully.
        Ok(())
    }

    fn stop_consolidation(&self) {
        // P1: No-op. Scheduling is managed by the Runtime.
    }

    async fn notify_consolidation_active(&self) {
        // P1: No-op. The Runtime's ConsolidationScheduler handles this.
    }

    fn get_pending_consolidation_count(&self) -> AcoworkResult<usize> {
        let pending = self
            .get_pending_for_consolidation(0, usize::MAX)
            .map_err(err_to_acowork)?;
        Ok(pending.len())
    }

    async fn run_offline_consolidation(
        &self,
        offline_config: &OfflineConsolidationConfig,
        llm: Option<&dyn acowork_memory::consolidation::TripleExtractorLlm>,
        embedding_fn: Option<Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>>,
        gen_config: Option<&GeneralizationConfig>,
    ) -> AcoworkResult<OfflineConsolidationResult> {
        let result = self
            .run_offline_consolidation_with_generalization(
                offline_config,
                llm,
                embedding_fn,
                gen_config,
            )
            .await
            .map_err(err_to_acowork)?;
        Ok(result)
    }

    // ── ADR-057 P0: Compaction distillation landing pipeline ────────────
    //
    // Override of the trait default (see
    // `acowork_memory::MemoryProvider::ingest_distilled_triples`). The trait
    // default cannot establish the `source_episode_id` reverse link or the
    // cross-layer `SOURCED_FROM` edge because `store_episode` does not expose
    // the NodeId (ADR-057 D4). GrafeoStore has the NodeId, so this override
    // runs the same instant pipeline (`process_memory_store`) per triple —
    // dedup / conflict detection / status dispatch semantics stay IDENTICAL
    // across providers — and additionally stamps `source_episode_id` (D4)
    // and creates the `Episodic -[SOURCED_FROM]-> Knowledge` edges (D9).

    async fn ingest_distilled_triples(
        &self,
        episode: &acowork_memory::DistilledEpisode,
        embedding_provider: Option<&dyn acowork_core::EmbeddingProvider>,
    ) -> AcoworkResult<IngestResult> {
        use crate::types::edge_types;

        // Step 1: store the episode (synchronously, never loses data).
        // Embedding failure degrades to storing without a vector (D1) — a
        // missing vector never blocks the summary from being retrievable.
        let episode_embedding =
            GrafeoStore::compute_text_embedding(&episode.summary, embedding_provider).await;
        let grafeo_episode = GrafeoEpisode {
            id: None,
            session_id: episode.session_id.clone(),
            turn_index: 0,
            role: "distilled".to_string(),
            content: episode.summary.clone(),
            embedding: episode_embedding,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            importance: 0.7,
        };

        let episode_id = self
            .store_episode_with_session(&grafeo_episode, &grafeo_episode.session_id)
            .map_err(err_to_acowork)?;

        // Step 2: land each triple through the SAME instant pipeline the
        // trait default uses (`process_memory_store`). This guarantees
        // object-aware dedup, conflict detection with `conflict_group_id`
        // tagging, Active/Pending dispatch by confidence, and within-batch
        // dedup — no semantic divergence between default and override.
        // The earlier "batch pre-load + inline cosine" optimization was
        // removed: it skipped object comparison (silently dropping
        // knowledge updates) and skipped conflict detection entirely.
        let mut knowledge_ids: Vec<u64> = Vec::with_capacity(episode.triples.len());
        let mut conflicts_detected: usize = 0;

        for triple in &episode.triples {
            let triple_text = format!("{} {} {}", triple.subject, triple.predicate, triple.object);
            let embedding =
                GrafeoStore::compute_text_embedding(&triple_text, embedding_provider).await;
            let input = MemoryStoreInput {
                content: triple_text,
                sub_type: triple.sub_type.clone(),
                subject: Some(triple.subject.clone()),
                predicate: Some(triple.predicate.clone()),
                object: Some(triple.object.clone()),
                confidence: Some(triple.confidence),
                source_episode_id: Some(episode_id.0),
                embedding,
                autobiographical: None,
            };

            match self.process_memory_store(&input) {
                Ok(Some(result)) => {
                    let new_id = NodeId(result.node_id);
                    conflicts_detected += result.conflict_resolutions.len();

                    // Step 3: cross-layer SOURCED_FROM edge (D9). Best-effort:
                    // an edge failure only loses graph diffusion for this
                    // node, never the node itself.
                    let props: Vec<(String, grafeo_common::types::Value)> = vec![];
                    if let Err(e) =
                        self.create_memory_edge(episode_id, new_id, edge_types::SOURCED_FROM, props)
                    {
                        tracing::warn!(
                            error = %e,
                            episode = episode_id.0,
                            knowledge = new_id.0,
                            "SOURCED_FROM edge creation failed (diffusion lost)"
                        );
                    }

                    knowledge_ids.push(new_id.0);
                }
                Ok(None) => {
                    // Duplicate — already covered by existing knowledge.
                    tracing::debug!(
                        subject = %triple.subject,
                        predicate = %triple.predicate,
                        "distilled triple deduplicated against existing knowledge"
                    );
                }
                Err(e) => {
                    // D1 failure degradation: a single-triple failure is a
                    // warning; the episode (and remaining triples) survive.
                    tracing::warn!(
                        error = %e,
                        subject = %triple.subject,
                        "distilled triple landing failed (skipped)"
                    );
                }
            }
        }

        Ok(IngestResult {
            episode_id: episode_id.0,
            knowledge_ids,
            conflicts_detected,
        })
    }
}
