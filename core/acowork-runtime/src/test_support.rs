//! Test support utilities for ADR-051 C5.
//!
//! Provides `InMemoryProvider` and `MockRagProvider` for testing the Runtime
//! without depending on GrafeoStore.
//!
//! - `InMemoryProvider`: HashMap-backed `MemoryProvider` impl with basic text
//!   matching and cosine-similarity vector search. No persistence, no graph
//!   expansion, no consolidation. Suitable for unit tests that exercise the
//!   retrieval/store pipeline.
//! - `MockRagProvider`: Pre-configured `RagProvider` impl for testing the
//!   dual-channel retrieval merge.
//!
//! Design ref: ADR-051 §4.4, §8.5

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use acowork_core::error::Result;
use acowork_core::rag::{AnnotatedRagResult, RagProvider, RagResultItem};
use acowork_memory::consolidation::{
    GeneralizationConfig, GeneralizationResult, MemoryStoreInput, MemoryStoreResult,
    OfflineConsolidationConfig, OfflineConsolidationResult, SchedulerConfig, TripleExtractorLlm,
};
use acowork_memory::types::{
    AutobioCategory, AutobiographicalNode, DecayConfig, DecayScanResult, Episode, KnowledgeNode,
    KnowledgeSubType, MemoryQuery, NodeStatus, ProceduralNode, PurgeResult, ResultSource,
    SearchResult, StoreHealth, StoreStats,
};
use acowork_memory::MemoryProvider;
use acowork_memory::quality::MemoryQualityConfig;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

// ============================================================================
// InMemoryProvider
// ============================================================================

/// Internal node representation for InMemoryProvider.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct InMemoryNode {
    id: u64,
    label: String,
    content: String,
    embedding: Option<Vec<f32>>,
    session_id: Option<String>,
    confidence: f32,
    status: NodeStatus,
    created_at: DateTime<Utc>,
}

/// A simple in-memory implementation of `MemoryProvider` for testing.
///
/// Uses `HashMap` storage with basic text matching (word overlap) and
/// cosine similarity for vector search. No persistence, no graph expansion,
/// no consolidation. Suitable for unit tests that exercise the
/// retrieval/store pipeline without pulling in GrafeoStore.
///
/// Design ref: ADR-051 §4.4, §8.5
pub struct InMemoryProvider {
    nodes: RwLock<HashMap<u64, InMemoryNode>>,
    episodes: RwLock<Vec<Episode>>,
    edges: RwLock<Vec<(u64, u64, String)>>,
    next_id: AtomicU64,
}

impl InMemoryProvider {
    /// Create a new empty provider.
    pub fn new() -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            episodes: RwLock::new(Vec::new()),
            edges: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Cosine similarity between two vectors.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        (dot / (norm_a * norm_b)) as f64
    }

    /// Simple text relevance: fraction of query words found in content.
    fn text_relevance(query: &str, content: &str) -> f64 {
        let query_words: Vec<&str> = query.split_whitespace().collect();
        if query_words.is_empty() {
            return 0.0;
        }
        let content_lower = content.to_lowercase();
        let matches = query_words
            .iter()
            .filter(|w| content_lower.contains(&w.to_lowercase()))
            .count();
        matches as f64 / query_words.len() as f64
    }
}

impl Default for InMemoryProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryProvider for InMemoryProvider {
    // ── Episodic layer ──────────────────────────────────────────────────

    fn store_episode(&self, episode: &Episode) -> Result<()> {
        self.episodes.write().unwrap().push(episode.clone());
        Ok(())
    }

    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        let episodes = self.episodes.read().unwrap();
        let results: Vec<SearchResult> = episodes
            .iter()
            .filter(|e| {
                query.query_text.is_empty()
                    || e.content
                        .to_lowercase()
                        .contains(&query.query_text.to_lowercase())
            })
            .take(query.limit)
            .map(|e| SearchResult {
                content: e.content.clone(),
                label: "Episodic".to_string(),
                score: 1.0,
                source: ResultSource::DirectMatch,
                context_tokens: e.content.len() / 4,
                node_id: 0,
                source_context: None,
            })
            .collect();
        Ok(results)
    }

    fn mark_consolidated(&self, _ids: &[u64]) -> Result<()> {
        Ok(())
    }

    fn cleanup_episodes(&self, _older_than: Duration) -> Result<u64> {
        Ok(0)
    }

    fn get_episodes(&self, session_id: Option<&str>, limit: usize) -> Result<Vec<Episode>> {
        let episodes = self.episodes.read().unwrap();
        let filtered: Vec<Episode> = episodes
            .iter()
            .filter(|e| session_id.is_none_or(|sid| e.session_id == sid))
            .rev()
            .take(limit)
            .cloned()
            .collect();
        Ok(filtered)
    }

    // ── Semantic layer ──────────────────────────────────────────────────

    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()> {
        let id = self.alloc_id();
        let content = format!("{} {} {}", node.subject, node.predicate, node.object);
        self.nodes.write().unwrap().insert(
            id,
            InMemoryNode {
                id,
                label: "Knowledge".to_string(),
                content,
                embedding: node.embedding.clone(),
                session_id: None,
                confidence: node.confidence,
                status: node.status.clone(),
                created_at: node.created_at,
            },
        );
        Ok(())
    }

    fn store_procedural(&self, node: &ProceduralNode) -> Result<()> {
        let id = node.id.unwrap_or_else(|| self.alloc_id());
        let content = format!("{}: {}", node.trigger_condition, node.action_pattern);
        // Memory contract: Vec (empty = no vector); storage side is Option.
        let embedding = if node.embedding.is_empty() {
            None
        } else {
            Some(node.embedding.clone())
        };
        self.nodes.write().unwrap().insert(
            id,
            InMemoryNode {
                id,
                label: "Procedural".to_string(),
                content,
                embedding,
                session_id: None,
                confidence: node.confidence,
                status: node.status.clone(),
                created_at: node.created_at,
            },
        );
        Ok(())
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        let id = node.id.unwrap_or_else(|| self.alloc_id());
        let content = format!("{}: {}: {}", node.category.as_str(), node.key, node.value);
        self.nodes.write().unwrap().insert(
            id,
            InMemoryNode {
                id,
                label: "Autobiographical".to_string(),
                content,
                embedding: node.embedding.clone(),
                session_id: None,
                confidence: node.confidence,
                status: NodeStatus::Active,
                created_at: node.created_at,
            },
        );
        Ok(())
    }

    // ── Unified retrieval ───────────────────────────────────────────────

    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        let nodes = self.nodes.read().unwrap();
        let results: Vec<SearchResult> = nodes
            .values()
            .filter(|n| {
                query.query_text.is_empty()
                    || Self::text_relevance(&query.query_text, &n.content) > 0.0
            })
            .take(query.limit)
            .map(|n| SearchResult {
                content: n.content.clone(),
                label: n.label.clone(),
                score: Self::text_relevance(&query.query_text, &n.content),
                source: ResultSource::DirectMatch,
                context_tokens: n.content.len() / 4,
                node_id: n.id,
                source_context: None,
            })
            .collect();
        Ok(results)
    }

    fn graph_expand(&self, _seeds: &[SearchResult], _hops: u8) -> Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }

    // ── Forgetting ──────────────────────────────────────────────────────

    fn run_decay_scan(&self, _config: &DecayConfig) -> Result<DecayScanResult> {
        Ok(DecayScanResult::default())
    }

    fn reactivate_node(&self, _node_id: u64) -> Result<()> {
        Ok(())
    }

    fn purge_expired(&self, _max_dormant_age: Duration) -> Result<PurgeResult> {
        Ok(PurgeResult::default())
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    fn health_check(&self) -> Result<StoreHealth> {
        Ok(StoreHealth {
            is_healthy: true,
            latency_ms: 0,
            error_count: 0,
            details: None,
        })
    }

    fn stats(&self) -> Result<StoreStats> {
        let nodes = self.nodes.read().unwrap();
        let episodes = self.episodes.read().unwrap();
        Ok(StoreStats {
            episode_count: episodes.len() as u64,
            node_count: nodes.len() as u64,
            active_node_count: nodes
                .values()
                .filter(|n| n.status == NodeStatus::Active)
                .count() as u64,
            dormant_node_count: 0,
            edge_count: self.edges.read().unwrap().len() as u64,
            storage_size_bytes: 0,
            index_count: 0,
        })
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }

    // ── Phase 1: Hybrid retrieval (extended) ────────────────────────────

    fn hybrid_search_full(
        &self,
        label: &str,
        query_text: &str,
        embedding: &[f32],
        k: usize,
        text_weight: f64,
        vector_weight: f64,
        min_score: Option<f32>,
    ) -> Result<Vec<(u64, f64)>> {
        let nodes = self.nodes.read().unwrap();
        let mut results: Vec<(u64, f64)> = nodes
            .values()
            .filter(|n| n.label == label)
            .map(|n| {
                let text_score = Self::text_relevance(query_text, &n.content);
                let vec_score = if !embedding.is_empty() {
                    n.embedding
                        .as_ref()
                        .map_or(0.0, |emb| Self::cosine_similarity(embedding, emb))
                } else {
                    0.0
                };
                let score = text_score * text_weight + vec_score * vector_weight;
                (n.id, score)
            })
            .filter(|(_, score)| min_score.is_none_or(|ms| *score as f32 >= ms))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        Ok(results)
    }

    fn text_search_with_filter(
        &self,
        label: &str,
        _field: &str,
        query_text: &str,
        k: usize,
        min_score: Option<f32>,
    ) -> Result<Vec<(u64, f64)>> {
        let nodes = self.nodes.read().unwrap();
        let mut results: Vec<(u64, f64)> = nodes
            .values()
            .filter(|n| n.label == label)
            .map(|n| (n.id, Self::text_relevance(query_text, &n.content)))
            .filter(|(_, score)| min_score.is_none_or(|ms| *score as f32 >= ms))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(k);
        Ok(results)
    }

    // ── memory_store tool entry ─────────────────────────────────────────

    fn process_memory_store(&self, input: &MemoryStoreInput) -> Result<Option<MemoryStoreResult>> {
        let id = self.alloc_id();
        let content = input.content.clone();
        let confidence = input.confidence.unwrap_or(0.7);
        let status = if confidence >= 0.85 {
            NodeStatus::Active
        } else {
            NodeStatus::Pending
        };
        self.nodes.write().unwrap().insert(
            id,
            InMemoryNode {
                id,
                label: "Knowledge".to_string(),
                content,
                embedding: input.embedding.clone(),
                session_id: None,
                confidence,
                status,
                created_at: Utc::now(),
            },
        );
        Ok(Some(MemoryStoreResult {
            node_id: id,
            conflict_resolutions: Vec::new(),
        }))
    }
    // ── Ambiguous conflict confirmation ────────────────────────────────

    fn should_trigger_confirmation(&self) -> Result<bool> {
        Ok(false)
    }

    fn generate_confirmation_hint(&self) -> Result<Option<String>> {
        Ok(None)
    }

    // ── Experience generalization (Path C) ─────────────────────────────

    async fn run_generalization(
        &self,
        _session_id: Option<&str>,
        _embedding_fn: &Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>,
        _config: &GeneralizationConfig,
    ) -> Result<GeneralizationResult> {
        Ok(GeneralizationResult {
            patterns: Vec::new(),
            nodes_created: 0,
            nodes_boosted: 0,
            patterns_deduplicated: 0,
            generalized_at: Utc::now(),
        })
    }

    fn compress_history_nodes(&self, _keep_recent: usize) -> Result<usize> {
        Ok(0)
    }

    // ── Node CRUD ───────────────────────────────────────────────────────

    fn get_all_procedural_nodes(&self) -> Result<Vec<ProceduralNode>> {
        Ok(Vec::new())
    }

    fn find_procedural_by_trigger(
        &self,
        _trigger: &str,
        _limit: usize,
    ) -> Result<Vec<ProceduralNode>> {
        Ok(Vec::new())
    }

    fn get_procedural(&self, _node_id: u64) -> Result<Option<ProceduralNode>> {
        Ok(None)
    }

    fn update_procedural(&self, _node: &ProceduralNode) -> Result<()> {
        Ok(())
    }

    fn find_autobiographical_by_key(&self, _key: &str) -> Result<Option<AutobiographicalNode>> {
        Ok(None)
    }

    fn find_autobiographical_by_category(
        &self,
        _category: AutobioCategory,
    ) -> Result<Vec<AutobiographicalNode>> {
        Ok(Vec::new())
    }

    fn update_autobiographical(&self, _node: &AutobiographicalNode) -> Result<()> {
        Ok(())
    }

    fn create_memory_edge(
        &self,
        from: u64,
        to: u64,
        edge_type: &str,
        _properties: Vec<(&str, String)>,
    ) -> Result<()> {
        self.edges
            .write()
            .unwrap()
            .push((from, to, edge_type.to_string()));
        Ok(())
    }

    // ── Phase 1 C4: Retrieval pipeline support ─────────────────────────

    fn graph_expand_seeded(
        &self,
        _seeds: &[(u64, f64)],
        _hint_type: &str,
    ) -> Result<Vec<(u64, f64, String)>> {
        Ok(Vec::new())
    }

    fn get_node_content(&self, node_id: u64) -> Result<Option<String>> {
        Ok(self
            .nodes
            .read()
            .unwrap()
            .get(&node_id)
            .map(|n| n.content.clone()))
    }

    fn get_node_session_id(&self, node_id: u64) -> Result<Option<String>> {
        Ok(self
            .nodes
            .read()
            .unwrap()
            .get(&node_id)
            .and_then(|n| n.session_id.clone()))
    }

    fn get_node_status(&self, node_id: u64) -> Result<Option<NodeStatus>> {
        Ok(self
            .nodes
            .read()
            .unwrap()
            .get(&node_id)
            .map(|n| n.status.clone()))
    }

    fn get_node_created_at(&self, node_id: u64) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .nodes
            .read()
            .unwrap()
            .get(&node_id)
            .map(|n| n.created_at))
    }

    fn apply_pagerank_boost(&self, _scores: &mut [(u64, f64)], _weight: f64) -> Result<()> {
        // No graph topology in InMemoryProvider - no-op.
        Ok(())
    }

    fn apply_quality_config(&self, _config: &MemoryQualityConfig) -> Result<()> {
        // InMemoryProvider is a test stub without dedup/consolidation
        // thresholds - no-op (zero-config behaviour is identical either way).
        Ok(())
    }

    // ── Consolidation lifecycle ────────────────────────────────────────

    fn start_consolidation(&self, _config: &SchedulerConfig) -> Result<()> {
        Ok(())
    }

    fn stop_consolidation(&self) {}

    async fn notify_consolidation_active(&self) {}

    fn get_pending_consolidation_count(&self) -> Result<usize> {
        Ok(0)
    }

    async fn run_offline_consolidation(
        &self,
        _offline_config: &OfflineConsolidationConfig,
        _llm: Option<&dyn TripleExtractorLlm>,
        _embedding_fn: Option<Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>>,
        _gen_config: Option<&GeneralizationConfig>,
    ) -> Result<OfflineConsolidationResult> {
        Ok(OfflineConsolidationResult::default())
    }
}

// ============================================================================
// MockRagProvider
// ============================================================================

/// A mock `RagProvider` for testing the dual-channel retrieval merge.
///
/// Returns pre-configured results regardless of query content.
///
/// Design ref: ADR-051 §8.5
pub struct MockRagProvider {
    results: Vec<RagResultItem>,
    provider_name: String,
}

impl MockRagProvider {
    /// Create a mock provider with pre-configured results.
    pub fn new(results: Vec<RagResultItem>, provider_name: &str) -> Self {
        Self {
            results,
            provider_name: provider_name.to_string(),
        }
    }

    /// Create a mock provider that returns empty results.
    pub fn empty() -> Self {
        Self {
            results: Vec::new(),
            provider_name: "mock_rag".to_string(),
        }
    }
}

impl Default for MockRagProvider {
    fn default() -> Self {
        Self::empty()
    }
}

#[async_trait]
impl RagProvider for MockRagProvider {
    async fn query(&self, _query_text: &str) -> Vec<AnnotatedRagResult> {
        self.results
            .iter()
            .map(|item| AnnotatedRagResult {
                item: item.clone(),
                source_label: format!("[RAG:{}]", self.provider_name),
                tool_name: self.provider_name.clone(),
            })
            .collect()
    }

    async fn query_with_params(
        &self,
        _query_text: &str,
        top_k: Option<u32>,
        _score_threshold: Option<f32>,
        _filters: Option<serde_json::Value>,
    ) -> Vec<AnnotatedRagResult> {
        let limit = top_k.unwrap_or(10) as usize;
        self.results
            .iter()
            .take(limit)
            .map(|item| AnnotatedRagResult {
                item: item.clone(),
                source_label: format!("[RAG:{}]", self.provider_name),
                tool_name: self.provider_name.clone(),
            })
            .collect()
    }

    fn name(&self) -> &str {
        &self.provider_name
    }
}

// ============================================================================
// Tests for the test support itself
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inmemory_provider_store_and_retrieve() {
        let provider = InMemoryProvider::new();

        // Store a memory via process_memory_store.
        let input = MemoryStoreInput {
            content: "User lives in Shanghai".to_string(),
            sub_type: KnowledgeSubType::Fact,
            subject: None,
            predicate: None,
            object: None,
            confidence: Some(0.9),
            source_episode_id: None,
            embedding: None,
            privacy: None,
            importance: None,
            keywords: None,
            autobiographical: None,
        };
        let result = provider.process_memory_store(&input).unwrap();
        assert!(result.is_some());
        let node_id = result.unwrap().node_id;
        assert!(node_id > 0);

        // Retrieve content by node_id.
        let content = provider.get_node_content(node_id).unwrap();
        assert_eq!(content.as_deref(), Some("User lives in Shanghai"));

        // Text search should find it.
        let search_results = provider
            .text_search_with_filter("Knowledge", "content", "Shanghai", 10, None)
            .unwrap();
        assert_eq!(search_results.len(), 1);
        assert_eq!(search_results[0].0, node_id);
    }

    #[test]
    fn test_inmemory_provider_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((InMemoryProvider::cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!((InMemoryProvider::cosine_similarity(&a, &c) - 0.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_mock_rag_provider_returns_configured_results() {
        let results = vec![RagResultItem {
            content: "Q3 roadmap includes AI assistant".to_string(),
            source_url: Some("https://example.com/roadmap".to_string()),
            chunk_id: Some("chunk-1".to_string()),
            score: 0.92,
        }];
        let provider = MockRagProvider::new(results, "enterprise_kb");

        let annotated = provider.query("roadmap").await;
        assert_eq!(annotated.len(), 1);
        assert_eq!(annotated[0].item.content, "Q3 roadmap includes AI assistant");
        assert_eq!(annotated[0].source_label, "[RAG:enterprise_kb]");
        assert_eq!(annotated[0].tool_name, "enterprise_kb");
    }

    #[tokio::test]
    async fn test_mock_rag_provider_empty() {
        let provider = MockRagProvider::empty();
        let annotated = provider.query("anything").await;
        assert!(annotated.is_empty());
        assert_eq!(provider.name(), "mock_rag");
    }
}
