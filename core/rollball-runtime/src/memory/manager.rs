//! MemoryManager — orchestrates the three-phase memory lifecycle.
//!
//! 1. Retrieve — search relevant memories before LLM generation
//! 2. Inject  — format and inject memories into the system prompt
//! 3. Record  — asynchronously record the conversation episode
//!
//! Phase 4 (S4.5): When manifest declares RAG, retrieve() runs dual-channel:
//! Grafeo (local) + RAG (enterprise) in parallel, with source annotations.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use grafeo_common::types::{NodeId, Timestamp, Value};
use rollball_grafeo::{
    grafeo::GrafeoStore,
    spreading::{config_from_hint, get_hint_weights},
    types::labels,
};
use rollball_memory::{HintType, MemoryQuery, RetrievalMetrics};

use crate::error::{Result, RuntimeError};
use crate::tools::rag::client::RagClient;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for MemoryManager.
#[derive(Debug, Clone)]
pub struct MemoryManagerConfig {
    /// Token budget for memory injection (default: 2000).
    pub max_inject_tokens: usize,
    /// Default number of results to retrieve (default: 10).
    pub default_k: usize,
    /// Default abstention threshold (default: 0.0 — no filtering;
    /// RRF scores from hybrid search are typically 0.01-0.05,
    /// so a non-zero default would filter everything).
    pub default_min_score: f32,
    /// Enable graph expansion (default: true).
    pub enable_graph_expand: bool,
    /// Record episodes asynchronously (default: true).
    pub record_async: bool,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            max_inject_tokens: 2000,
            default_k: 10,
            default_min_score: 0.0,
            enable_graph_expand: true,
            record_async: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Result of a memory retrieval operation.
#[derive(Debug)]
pub struct RetrievalResult {
    /// Retrieved memories sorted by relevance (highest first).
    pub memories: Vec<RetrievedMemory>,
    /// Metrics collected during retrieval.
    pub metrics: RetrievalMetrics,
}

/// A single retrieved memory with relevance metadata.
#[derive(Debug, Clone)]
pub struct RetrievedMemory {
    /// Formatted content text.
    pub content: String,
    /// Node label (Knowledge, Episodic, Procedural, Autobiographical) or
    /// RAG source label (e.g., "RAG:enterprise_knowledge").
    pub label: String,
    /// Relevance score.
    pub score: f64,
    /// Retrieval source: "vector" | "text" | "graph" | "hybrid" | "rag".
    pub source: String,
    /// Grafeo node ID (for tracing). 0 for RAG results.
    pub node_id: u64,
    /// Source URL (for RAG results, describing where the chunk came from).
    pub source_url: Option<String>,
    /// Chunk ID within the source document (for RAG results).
    pub chunk_id: Option<String>,
}

/// Formatted memory block ready for prompt injection.
#[derive(Debug)]
pub struct InjectedMemory {
    /// Ready to insert into system prompt.
    pub formatted_text: String,
    /// Approximate token count.
    pub token_count: usize,
    /// Number of memories included.
    pub memory_count: usize,
    /// Whether results were truncated by token budget.
    pub truncated: bool,
}

/// Record of a conversation turn for episodic storage.
#[derive(Debug)]
pub struct ConversationRecord {
    /// Session identifier.
    pub session_id: String,
    /// Turn index within the session.
    pub turn_index: u32,
    /// User message text.
    pub user_message: String,
    /// Assistant response text.
    pub assistant_response: String,
    /// IDs of memories used in this turn.
    pub retrieved_memory_ids: Vec<String>,
    /// Timestamp of the turn.
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// MemoryManager
// ---------------------------------------------------------------------------

/// Orchestrates the three-phase memory lifecycle.
///
/// When `rag_client` is Some, retrieve() runs dual-channel:
/// Grafeo (local) + RAG (enterprise) in parallel.
pub struct MemoryManager {
    config: MemoryManagerConfig,
    /// Optional RAG client for enterprise knowledge retrieval (Phase 4 S4.5).
    /// None = no RAG declared in manifest, behavior identical to Phase 3.
    rag_client: Option<Arc<RagClient>>,
}

impl MemoryManager {
    /// Create a new MemoryManager with the given configuration.
    pub fn new(config: MemoryManagerConfig) -> Self {
        Self {
            config,
            rag_client: None,
        }
    }

    /// Create a new MemoryManager with RAG support.
    ///
    /// When rag_client is Some, retrieve() will run dual-channel retrieval.
    pub fn with_rag(config: MemoryManagerConfig, rag_client: Arc<RagClient>) -> Self {
        Self {
            config,
            rag_client: Some(rag_client),
        }
    }

    /// Check if this manager has RAG enabled
    pub fn has_rag(&self) -> bool {
        self.rag_client.is_some()
    }

    /// Retrieve relevant memories for the current query.
    ///
    /// Pipeline: Grafeo hybrid_search → graph_expand → merge & rank
    /// + RAG channel (if rag_client is Some, run in parallel).
    ///
    /// RAG channel uses the user message as query with default top_k=3.
    /// Results from both channels are merged and sorted by score.
    /// Source annotations distinguish [Grafeo] vs [RAG:<tool_name>].
    pub async fn retrieve(&self, store: &GrafeoStore, query: &MemoryQuery) -> Result<RetrievalResult> {
        let k = if query.limit > 0 { query.limit } else { self.config.default_k };
        let min_score = query.min_score.unwrap_or(self.config.default_min_score);
        let hint_type = query.hint_type;
        let (vector_weight, text_weight, _graph_weight) = get_hint_weights(hint_type.as_str());

        // Determine which labels to search based on hint type.
        let search_labels: Vec<&str> = match hint_type {
            HintType::Identity => vec![labels::AUTOBIOGRAPHICAL],
            _ => vec![
                labels::EPISODIC,
                labels::KNOWLEDGE,
                labels::PROCEDURAL,
                labels::AUTOBIOGRAPHICAL,
            ],
        };

        // Run hybrid search on each label.
        let mut all_results: Vec<(u64, f64, String, String)> = Vec::new();

        for label in &search_labels {
            let search_result = if let Some(ref embedding) = query.embedding {
                store.hybrid_search_full(
                    label,
                    &query.query_text,
                    embedding,
                    k,
                    text_weight,
                    vector_weight,
                    Some(min_score),
                )
                .map_err(|e| RuntimeError::Tool(format!("Hybrid search failed: {e}")))
            } else {
                // Fallback to text search when no embedding is available.
                store.text_search_with_filter(label, "content", &query.query_text, k, Some(min_score))
                    .map_err(|e| RuntimeError::Tool(format!("Text search failed: {e}")))
            };

            match search_result {
                Ok(results) => {
                    for (node_id, score) in results {
                        let source = if query.embedding.is_some() {
                            "hybrid".to_string()
                        } else {
                            "text".to_string()
                        };
                        all_results.push((node_id.as_u64(), score, label.to_string(), source));
                    }
                }
                Err(e) => {
                    // Log and continue — partial results are better than no results.
                    tracing::warn!("Search failed for label {}: {}", label, e);
                }
            }
        }

        // Graph expansion (if enabled and we have seed results).
        let mut graph_expand_count = 0;
        if self.config.enable_graph_expand && !all_results.is_empty() {
            let expand_config = config_from_hint(hint_type.as_str());
            let seeds: Vec<(NodeId, f64)> = all_results
                .iter()
                .map(|(id, score, _, _)| (NodeId::new(*id), *score))
                .collect();

            match store.graph_expand(&seeds, &expand_config)
                .map_err(|e| RuntimeError::Tool(format!("Graph expand failed: {e}"))) {
                Ok(expanded) => {
                    graph_expand_count = expanded.len();
                    for node in expanded {
                        all_results.push((
                            node.node_id.as_u64(),
                            node.accumulated_score,
                            node.label,
                            "graph".to_string(),
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!("Graph expand failed: {}", e);
                }
            }
        }

        // Deduplicate by node_id, keeping the highest score.
        let mut best_by_id: HashMap<u64, (f64, String, String)> = HashMap::new();
        for (id, score, label, source) in all_results {
            best_by_id
                .entry(id)
                .and_modify(|(existing_score, existing_label, existing_source)| {
                    if score > *existing_score {
                        *existing_score = score;
                        *existing_label = label.clone();
                        *existing_source = source.clone();
                    }
                })
                .or_insert((score, label, source));
        }

        // Build RetrievedMemory list, sorted by score descending.
        let mut memories: Vec<RetrievedMemory> = Vec::new();
        for (node_id, (score, label, source)) in best_by_id {
            let content = extract_node_content(store, node_id);
            memories.push(RetrievedMemory {
                content,
                label,
                score,
                source,
                node_id,
                source_url: None,
                chunk_id: None,
            });
        }

        // RAG channel: parallel query when rag_client is configured (S4.5)
        //
        // Uses the user message as query with top_k=3 (lightweight).
        // RAG unavailability is non-blocking — timeout returns empty.
        let mut rag_result_count = 0;
        if let Some(ref rag_client) = self.rag_client {
            let rag_results = rag_client.query(&query.query_text).await;
            rag_result_count = rag_results.len();
            for annotated in rag_results {
                memories.push(RetrievedMemory {
                    content: annotated.item.content,
                    label: annotated.source_label.trim_start_matches('[').trim_end_matches(']').to_string(),
                    score: annotated.item.score as f64,
                    source: "rag".to_string(),
                    node_id: 0, // RAG results have no Grafeo node
                    source_url: annotated.item.source_url,
                    chunk_id: annotated.item.chunk_id,
                });
            }
        }
        memories.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Limit to k results.
        let result_count = memories.len().min(k);
        memories.truncate(result_count);

        // Compute metrics.
        let max_score = memories.iter().map(|m| m.score as f32).fold(0.0f32, f32::max);
        let avg_score = if result_count > 0 {
            memories.iter().map(|m| m.score as f32).sum::<f32>() / result_count as f32
        } else {
            0.0
        };
        let abstention_triggered = result_count == 0 && query.abstention_enabled;

        let metrics = RetrievalMetrics {
            result_count,
            avg_score,
            max_score,
            abstention_triggered,
            filtered_count: 0,
            retrieval_level: 0,
            graph_expand_nodes: graph_expand_count,
            hint_type: query.hint_type,
        };

        tracing::debug!(
            "Retrieved {} memories (max_score={:.3}, avg_score={:.3}, graph_expanded={}, rag_results={})",
            result_count,
            max_score,
            avg_score,
            graph_expand_count,
            rag_result_count
        );

        Ok(RetrievalResult { memories, metrics })
    }

    /// Format retrieved memories for system prompt injection.
    ///
    /// Respects token budget, prioritizes by score.
    pub fn inject(&self, retrieval: &RetrievalResult, max_tokens: usize) -> InjectedMemory {
        if retrieval.memories.is_empty() {
            return InjectedMemory {
                formatted_text: String::new(),
                token_count: 0,
                memory_count: 0,
                truncated: false,
            };
        }

        let mut lines: Vec<String> = Vec::new();
        let mut token_count: usize = 0;
        let mut truncated = false;

        for memory in &retrieval.memories {
            let line = format!("[{}] {}", memory.label, memory.content);
            let line_tokens = estimate_tokens(&line);

            if token_count + line_tokens > max_tokens && !lines.is_empty() {
                truncated = true;
                break;
            }

            lines.push(line);
            token_count += line_tokens;

            // If a single memory exceeds the budget on an empty list, include it anyway
            // but mark truncated.
            if token_count > max_tokens && lines.len() == 1 {
                truncated = true;
            }
        }

        let formatted_text = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n")
        };

        InjectedMemory {
            formatted_text,
            token_count,
            memory_count: lines.len(),
            truncated,
        }
    }

    /// Record a conversation turn as an episode.
    ///
    /// In production this runs asynchronously; for now synchronous.
    pub fn record(&self, store: &GrafeoStore, record: &ConversationRecord) -> Result<()> {
        let content = format!(
            "User: {}\nAssistant: {}",
            record.user_message, record.assistant_response
        );

        let mut props = vec![
            ("session_id", Value::from(record.session_id.as_str())),
            ("turn_index", Value::from(i64::from(record.turn_index))),
            ("role", Value::from("conversation")),
            ("content", Value::from(content.as_str())),
            ("content_type", Value::from("Informational")),
            (
                "timestamp",
                Value::from(Timestamp::from_micros(
                    record.timestamp.timestamp_micros(),
                )),
            ),
            ("consolidated", Value::from(false)),
        ];

        // Store retrieved memory IDs as metadata.
        if !record.retrieved_memory_ids.is_empty() {
            let ids_json = serde_json::to_string(&record.retrieved_memory_ids)
                .map_err(RuntimeError::Json)?;
            props.push(("metadata", Value::from(ids_json.as_str())));
        }

        store
            .store_node(labels::EPISODIC, props)
            .map_err(|e| RuntimeError::Tool(format!("Failed to record episode: {e}")))?;

        Ok(())
    }

    /// Full memory lifecycle for a single turn:
    /// 1. Retrieve memories for the query
    /// 2. Format for injection
    /// 3. Return injection text + metrics
    pub async fn process_turn(
        &self,
        store: &GrafeoStore,
        query: &MemoryQuery,
    ) -> Result<(InjectedMemory, RetrievalMetrics)> {
        let retrieval = self.retrieve(store, query).await?;
        let metrics = retrieval.metrics.clone();
        let injected = self.inject(&retrieval, self.config.max_inject_tokens);
        Ok((injected, metrics))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract human-readable content from a Grafeo node.
fn extract_node_content(store: &GrafeoStore, node_id: u64) -> String {
    let nid = NodeId::new(node_id);
    let Some(node) = store.get_node(nid) else {
        return String::new();
    };

    // Try common content fields in priority order.
    if let Some(content) = node.get_property("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }
    if let Some(value) = node.get_property("value").and_then(|v| v.as_str()) {
        return value.to_string();
    }
    if let Some(action) = node.get_property("action_pattern").and_then(|v| v.as_str()) {
        return action.to_string();
    }

    // Knowledge nodes: combine subject + predicate + object.
    let subject = node.get_property("subject").and_then(|v| v.as_str());
    let predicate = node.get_property("predicate").and_then(|v| v.as_str());
    let object = node.get_property("object").and_then(|v| v.as_str());

    if let (Some(s), Some(p), Some(o)) = (subject, predicate, object) {
        return format!("{s} {p} {o}");
    }

    // Procedural nodes: combine trigger + action.
    let trigger = node.get_property("trigger_condition").and_then(|v| v.as_str());
    let action = node.get_property("action_pattern").and_then(|v| v.as_str());
    if let (Some(t), Some(a)) = (trigger, action) {
        return format!("When {t}: {a}");
    }

    // Fallback: use any string property.
    for key in ["name", "key", "description"] {
        if let Some(v) = node.get_property(key).and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }

    String::new()
}

/// Simple token estimation heuristic.
///
/// - ASCII characters: ~4 chars per token
/// - Non-ASCII (CJK, etc.): ~2 chars per token
fn estimate_tokens(text: &str) -> usize {
    let ascii_count = text.chars().filter(|c| c.is_ascii()).count();
    let non_ascii_count = text.chars().count() - ascii_count;

    let ascii_tokens = ascii_count.div_ceil(4);
    let non_ascii_tokens = non_ascii_count.div_ceil(2);

    ascii_tokens + non_ascii_tokens
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use grafeo_common::types::Value;
    use rollball_grafeo::types::{labels, EMBEDDING_DIM};

    /// Helper: create an in-memory GrafeoStore for testing.
    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    /// Helper: generate a test embedding vector.
    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; EMBEDDING_DIM]
    }

    /// Helper: store an Episodic node with content and embedding.
    fn store_episode(store: &GrafeoStore, content: &str, embedding: &[f32]) -> u64 {
        let id = store
            .store_node(labels::EPISODIC, [("content", Value::from(content))])
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    /// Helper: store a Knowledge node with embedding.
    fn store_knowledge(store: &GrafeoStore, subject: &str, predicate: &str, object: &str, embedding: &[f32]) -> u64 {
        let id = store
            .store_node(
                labels::KNOWLEDGE,
                [
                    ("subject", Value::from(subject)),
                    ("predicate", Value::from(predicate)),
                    ("object", Value::from(object)),
                    ("sub_type", Value::from("Fact")),
                    ("confidence", Value::from(0.9f64)),
                    ("status", Value::from("Active")),
                ],
            )
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    /// Helper: store an Autobiographical node.
    #[allow(dead_code)]
    fn store_autobiographical(store: &GrafeoStore, key: &str, value: &str, embedding: &[f32]) -> u64 {
        let id = store
            .store_node(
                labels::AUTOBIOGRAPHICAL,
                [
                    ("category", Value::from("Identity")),
                    ("key", Value::from(key)),
                    ("value", Value::from(value)),
                    ("confidence", Value::from(1.0f64)),
                    ("status", Value::from("Active")),
                ],
            )
            .unwrap();
        store.db().set_node_property(
            id,
            "embedding",
            Value::Vector(std::sync::Arc::from(embedding.to_vec().into_boxed_slice())),
        );
        id.as_u64()
    }

    // =====================================================================
    // Test 1: Config defaults
    // =====================================================================

    #[test]
    fn test_config_defaults() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.max_inject_tokens, 2000);
        assert_eq!(config.default_k, 10);
        assert_eq!(config.default_min_score, 0.0);
        assert!(config.enable_graph_expand);
        assert!(config.record_async);
    }

    // =====================================================================
    // Test 2: MemoryManager new
    // =====================================================================

    #[test]
    fn test_manager_new() {
        let config = MemoryManagerConfig::default();
        let manager = MemoryManager::new(config.clone());
        assert_eq!(manager.config.max_inject_tokens, config.max_inject_tokens);
    }

    // =====================================================================
    // Test 3: Retrieve normal case
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_normal() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "user likes rust programming", &emb);
        store_knowledge(&store, "user", "lives_in", "Beijing", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &query).await.unwrap();
        assert!(!result.memories.is_empty(), "expected at least one result");
        assert!(!result.metrics.abstention_triggered);
    }

    // =====================================================================
    // Test 4: Retrieve empty results
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_empty() {
        let store = test_store();
        let emb = test_embedding();

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "something completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99), // Very high threshold — should filter everything.
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &query).await.unwrap();
        assert!(result.memories.is_empty());
        assert!(result.metrics.abstention_triggered);
        assert_eq!(result.metrics.result_count, 0);
    }

    // =====================================================================
    // Test 5: Retrieve abstention triggered
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_abstention() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "test content", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "unrelated query".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &query).await.unwrap();
        assert!(result.metrics.abstention_triggered);
    }

    // =====================================================================
    // Test 6: Retrieve without embedding falls back to text search
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_no_embedding_fallback() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "rust programming tutorial", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: None,
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &query).await.unwrap();
        // Text search should still find results.
        assert!(!result.memories.is_empty());
    }

    // =====================================================================
    // Test 7: Inject normal case
    // =====================================================================

    #[test]
    fn test_inject_normal() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "User likes Rust.".to_string(),
                    label: "Knowledge".to_string(),
                    score: 0.95,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Previous discussion about traits.".to_string(),
                    label: "Episodic".to_string(),
                    score: 0.85,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
            ],
            metrics: RetrievalMetrics::default(),
        };

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let injected = manager.inject(&retrieval, 1000);

        assert!(!injected.formatted_text.is_empty());
        assert!(injected.formatted_text.contains("[Knowledge]"));
        assert!(injected.formatted_text.contains("[Episodic]"));
        assert_eq!(injected.memory_count, 2);
        assert!(!injected.truncated);
    }

    // =====================================================================
    // Test 8: Inject with truncation
    // =====================================================================

    #[test]
    fn test_inject_truncation() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "User likes Rust programming language for systems development.".to_string(),
                    label: "Knowledge".to_string(),
                    score: 0.95,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Another very long memory content that takes up many tokens.".to_string(),
                    label: "Episodic".to_string(),
                    score: 0.85,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Third memory with even more text content to exceed token budget.".to_string(),
                    label: "Procedural".to_string(),
                    score: 0.75,
                    source: "hybrid".to_string(),
                    node_id: 3,
                    source_url: None,
                    chunk_id: None,
                },
            ],
            metrics: RetrievalMetrics::default(),
        };

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let injected = manager.inject(&retrieval, 5); // Very tight budget.

        assert!(injected.memory_count < retrieval.memories.len());
        assert!(injected.truncated);
        assert!(injected.token_count <= 5 + estimate_tokens(&retrieval.memories[0].content));
    }

    // =====================================================================
    // Test 9: Inject empty retrieval
    // =====================================================================

    #[test]
    fn test_inject_empty() {
        let retrieval = RetrievalResult {
            memories: vec![],
            metrics: RetrievalMetrics::default(),
        };

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let injected = manager.inject(&retrieval, 1000);

        assert!(injected.formatted_text.is_empty());
        assert_eq!(injected.memory_count, 0);
        assert_eq!(injected.token_count, 0);
        assert!(!injected.truncated);
    }

    // =====================================================================
    // Test 10: Record conversation
    // =====================================================================

    #[test]
    fn test_record_conversation() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        let record = ConversationRecord {
            session_id: "sess-1".to_string(),
            turn_index: 0,
            user_message: "Hello".to_string(),
            assistant_response: "Hi there!".to_string(),
            retrieved_memory_ids: vec!["mem-1".to_string()],
            timestamp: Utc::now(),
        };

        manager.record(&store, &record).unwrap();

        // Verify the episode was stored by searching.
        let text_results = store
            .text_search_with_filter(labels::EPISODIC, "content", "Hello", 5, None)
            .unwrap();
        assert!(!text_results.is_empty(), "expected recorded episode to be found");
    }

    // =====================================================================
    // Test 11: process_turn integration
    // =====================================================================

    #[tokio::test]
    async fn test_process_turn() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "user prefers concise replies", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "concise".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager.process_turn(&store, &query).await.unwrap();

        assert!(!injected.formatted_text.is_empty());
        assert!(metrics.result_count > 0);
        assert!(!metrics.abstention_triggered);
    }

    // =====================================================================
    // Test 12: process_turn with abstention
    // =====================================================================

    #[tokio::test]
    async fn test_process_turn_abstention() {
        let store = test_store();
        let emb = test_embedding();
        store_episode(&store, "some content", &emb);

        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let query = MemoryQuery {
            query_text: "completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager.process_turn(&store, &query).await.unwrap();

        assert!(metrics.abstention_triggered);
        assert_eq!(injected.memory_count, 0);
        assert!(injected.formatted_text.is_empty());
    }
}
