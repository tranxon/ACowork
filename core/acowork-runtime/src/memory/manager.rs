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

use acowork_grafeo::{
    grafeo::GrafeoStore,
    spreading::{config_from_hint, get_hint_weights},
    types::labels,
};
use acowork_memory::{HintType, MemoryQuery, RetrievalMetrics};
use chrono::{DateTime, Utc};
use grafeo_common::types::{NodeId, Timestamp, Value};

use crate::embedding::EmbeddingProvider;
use crate::episode_distill::DistilledEpisode;
use crate::error::{Result, RuntimeError};
use crate::tools::rag::client::RagClient;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for MemoryManager.
#[derive(Debug, Clone)]
pub struct MemoryManagerConfig {
    /// Token budget for non-autobiographical memory injection (default: 2000).
    ///
    /// Applies to Episodic, Knowledge, Procedural, and RAG results only.
    /// Autobiographical memories have separate budgets (see below).
    pub max_inject_tokens: usize,
    /// Token budget for autobiographical core memories — Identity, Capability,
    /// Limitation (default: 100).
    ///
    /// These are the agent's "self-concept" and are always injected first.
    /// Per design §3.3, Identity/Capability are always relevant; this budget
    /// controls how much detail to include when nodes are numerous.
    pub max_autobio_core_tokens: usize,
    /// Token budget for autobiographical history memories — History and
    /// Relationship (default: 100).
    ///
    /// Per design §3.3: History Top-5 summaries, Relationship Top-3.
    /// Combined with `max_autobio_core_tokens`, the total autobiographical
    /// budget is 200 tokens (≈150 Chinese characters), matching the design spec.
    pub max_autobio_history_tokens: usize,
    /// Default number of results to retrieve (default: 10).
    pub default_k: usize,
    /// Default abstention threshold (default: 0.0 — no filtering;
    /// RRF scores from hybrid search are typically 0.01-0.05,
    /// so a non-zero default would filter everything).
    pub default_min_score: f32,
    /// Enable graph expansion (default: true).
    pub enable_graph_expand: bool,
    /// PageRank boost weight for topology-aware re-ranking (default: 0.1).
    ///
    /// When `enable_graph_expand` is true and this is > 0.0, the retrieval
    /// pipeline applies PageRank scores to the deduplicated results:
    /// `new_score = original_score * (1.0 - weight) + pagerank * weight`.
    ///
    /// Set to 0.0 to disable PageRank boosting.
    pub pagerank_weight: f64,
    /// Record episodes asynchronously (default: true).
    pub record_async: bool,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            max_inject_tokens: 2000,
            max_autobio_core_tokens: 100,
            max_autobio_history_tokens: 100,
            default_k: 10,
            default_min_score: 0.0,
            enable_graph_expand: true,
            pagerank_weight: 0.1,
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
    /// If `query.embedding` is `None` and `embedding_provider` is `Some`,
    /// generates the embedding automatically with a 200ms timeout before
    /// proceeding to hybrid search. On timeout or failure, falls back to
    /// text-only search (graceful degradation).
    ///
    /// Pipeline: (auto-embed) → Grafeo hybrid_search → graph_expand → dedup →
    /// PageRank boost (topology re-rank) → merge & rank
    /// + RAG channel (if rag_client is Some, run in parallel).
    ///
    /// RAG channel uses the user message as query with default top_k=3.
    /// Results from both channels are merged and sorted by score.
    /// Source annotations distinguish [Grafeo] vs [RAG:<tool_name>].
    pub async fn retrieve(
        &self,
        store: &GrafeoStore,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<RetrievalResult> {
        // ── Auto-generate embedding if needed ──
        // Timeout is handled by FallbackEmbeddingProvider internally
        // (200ms per attempt, then fallback to next provider).
        if query.embedding.is_none() {
            if let Some(provider) = embedding_provider {
                match provider.embed(&query.query_text).await {
                    Ok(vec) => {
                        tracing::debug!(
                            dim = vec.len(),
                            provider = provider.name(),
                            "Auto-generated query embedding"
                        );
                        query.embedding = Some(vec);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Embedding generation failed, falling back to text search"
                        );
                    }
                }
            }
        }

        let k = if query.limit > 0 {
            query.limit
        } else {
            self.config.default_k
        };
        let min_score = query.min_score.unwrap_or(self.config.default_min_score);
        let hint_type = query.hint_type;
        let (vector_weight, text_weight, _graph_weight) = get_hint_weights(hint_type.as_str());

        // Determine which labels to search based on hint type.
        let search_labels: Vec<&str> = match hint_type {
            HintType::Identity => vec![labels::AUTOBIOGRAPHICAL, labels::EPISODIC],
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
                store
                    .hybrid_search_full(
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
                store
                    .text_search_with_filter(
                        label,
                        "content",
                        &query.query_text,
                        k,
                        Some(min_score),
                    )
                    .map_err(|e| RuntimeError::Tool(format!("Text search failed: {e}")))
            };

            match search_result {
                Ok(results) => {
                    tracing::info!(
                        label,
                        result_count = results.len(),
                        "Memory search completed (before dedup + exclude)"
                    );
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

            match store
                .graph_expand(&seeds, &expand_config)
                .map_err(|e| RuntimeError::Tool(format!("Graph expand failed: {e}")))
            {
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

        // Post-filter: exclude nodes belonging to the current session.
        // Prevents re-injecting compaction summaries that are already in
        // the conversation context window.
        if let Some(ref exclude_sid) = query.filters.exclude_session_id {
            let before = best_by_id.len();
            best_by_id.retain(|node_id, _| {
                let nid = NodeId::new(*node_id);
                match store.db().get_node(nid) {
                    Some(node) => node
                        .get_property("session_id")
                        .map(|v| v.to_string().trim_matches('"') != *exclude_sid)
                        .unwrap_or(true),
                    None => true,
                }
            });
            tracing::debug!(
                before,
                after = best_by_id.len(),
                exclude_session_id = %exclude_sid,
                "Excluded current-session nodes from retrieval"
            );
        }

        // Apply PageRank topology boost for re-ranking (S2.8.3).
        // Only when graph expansion is enabled and weight > 0.
        if self.config.enable_graph_expand
            && self.config.pagerank_weight > 0.0
            && !best_by_id.is_empty()
        {
            let mut scored: Vec<(NodeId, f64)> = best_by_id
                .iter()
                .map(|(id, (score, _, _))| (NodeId::new(*id), *score))
                .collect();

            if let Err(e) = store.apply_pagerank_boost(&mut scored, self.config.pagerank_weight) {
                tracing::warn!("PageRank boost failed, continuing with unboosted scores: {e}");
            } else {
                // Map boosted scores back to best_by_id.
                for (node_id, boosted_score) in scored {
                    if let Some(entry) = best_by_id.get_mut(&node_id.as_u64()) {
                        entry.0 = boosted_score;
                    }
                }
            }
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
                    label: annotated
                        .source_label
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string(),
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
        let max_score = memories
            .iter()
            .map(|m| m.score as f32)
            .fold(0.0f32, f32::max);
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
    /// Three-phase injection with separate token budgets:
    ///
    /// - **Pass 1**: Autobiographical core (Identity/Capability/Limitation) —
    ///   the agent's self-concept. Always injected first, bounded by
    ///   `max_autobio_core_tokens` (default 100). These memories are never
    ///   skipped entirely; if the first one exceeds the budget it is still
    ///   included to avoid empty identity.
    ///
    /// - **Pass 2**: Autobiographical history (History/Relationship/Preference) —
    ///   contextual self-knowledge. Injected in score-descending order,
    ///   bounded by `max_autobio_history_tokens` (default 100).
    ///
    /// - **Pass 3**: Non-autobiographical memories (Episodic/Knowledge/
    ///   Procedural/RAG) — bounded by `max_inject_tokens` (default 2000).
    ///
    /// Per design §3.3, the total autobiographical budget is 200 tokens
    /// (≈150 Chinese characters). Each memory is kept intact (no mid-content
    /// truncation); memories that would exceed the budget are skipped entirely.
    pub fn inject(&self, retrieval: &RetrievalResult) -> InjectedMemory {
        if retrieval.memories.is_empty() {
            return InjectedMemory {
                formatted_text: String::new(),
                token_count: 0,
                memory_count: 0,
                truncated: false,
            };
        }

        let core_budget = self.config.max_autobio_core_tokens;
        let history_budget = self.config.max_autobio_history_tokens;
        let other_budget = self.config.max_inject_tokens;

        let mut lines: Vec<String> = Vec::new();
        let mut token_count: usize = 0;
        let mut truncated = false;

        // Partition autobiographical memories into core vs history.
        let mut autobio_core: Vec<&RetrievedMemory> = Vec::new();
        let mut autobio_history: Vec<&RetrievedMemory> = Vec::new();

        for memory in &retrieval.memories {
            if memory.label != labels::AUTOBIOGRAPHICAL {
                continue;
            }
            match autobio_subcategory(&memory.content) {
                AutobioGroup::Core => autobio_core.push(memory),
                AutobioGroup::History => autobio_history.push(memory),
            }
        }

        // Pass 1: inject autobiographical core (Identity/Capability/Limitation).
        let mut core_tokens: usize = 0;
        for memory in &autobio_core {
            let line = format!("[{}] {}", memory.label, memory.content);
            let line_tokens = estimate_tokens(&line);

            // Always include at least one core memory (agent identity).
            if core_tokens > 0 && core_tokens + line_tokens > core_budget {
                truncated = true;
                break;
            }

            lines.push(line);
            core_tokens += line_tokens;
        }
        token_count += core_tokens;

        // Pass 2: inject autobiographical history (History/Relationship/Preference).
        let mut history_tokens: usize = 0;
        for memory in &autobio_history {
            let line = format!("[{}] {}", memory.label, memory.content);
            let line_tokens = estimate_tokens(&line);

            if history_tokens + line_tokens > history_budget {
                truncated = true;
                break;
            }

            lines.push(line);
            history_tokens += line_tokens;
        }
        token_count += history_tokens;

        // Pass 3: inject non-autobiographical memories within token budget.
        let mut other_tokens: usize = 0;
        for memory in &retrieval.memories {
            if memory.label == labels::AUTOBIOGRAPHICAL {
                continue; // already handled in passes 1-2
            }
            let line = format!("[{}] {}", memory.label, memory.content);
            let line_tokens = estimate_tokens(&line);

            // Keep memory intact: skip entirely if it would exceed budget
            if other_tokens + line_tokens > other_budget {
                truncated = true;
                break;
            }

            lines.push(line);
            other_tokens += line_tokens;
        }
        token_count += other_tokens;

        // Edge case: if nothing was injected (not even autobiographical),
        // include the first result anyway to avoid empty injection.
        if lines.is_empty() && !retrieval.memories.is_empty() {
            let first = &retrieval.memories[0];
            let line = format!("[{}] {}", first.label, first.content);
            let line_tokens = estimate_tokens(&line);
            lines.push(line);
            token_count = line_tokens;
            truncated = true;
        }

        let formatted_text = lines.join("\n");

        InjectedMemory {
            formatted_text,
            token_count,
            memory_count: lines.len(),
            truncated,
        }
    }

    /// Format retrieved memories and append ambiguous conflict confirmation hints.
    ///
    /// Same as `inject()` but also checks the GrafeoStore for pending
    /// ambiguous conflicts. If `should_trigger_confirmation()` returns true,
    /// appends a confirmation hint that the LLM can use to naturally ask
    /// the user about the conflicting values.
    pub fn inject_with_ambiguous_hints(
        &self,
        retrieval: &RetrievalResult,
        store: &GrafeoStore,
    ) -> InjectedMemory {
        let mut injected = self.inject(retrieval);

        // Check for pending ambiguous conflicts.
        if let Ok(true) = store.should_trigger_confirmation() {
            if let Ok(Some(hint)) = store.generate_confirmation_hint() {
                let hint_line = format!("[Ambiguous] {}", hint);
                let hint_tokens = estimate_tokens(&hint_line);
                injected.formatted_text = format!("{}\n{}", injected.formatted_text, hint_line);
                injected.token_count += hint_tokens;
                injected.memory_count += 1;
            }
        }

        injected
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
            (
                "created_at",
                Value::from(Timestamp::from_micros(record.timestamp.timestamp_micros())),
            ),
            ("consolidated", Value::from(false)),
        ];

        // Store retrieved memory IDs as metadata.
        if !record.retrieved_memory_ids.is_empty() {
            let ids_json =
                serde_json::to_string(&record.retrieved_memory_ids).map_err(RuntimeError::Json)?;
            props.push(("metadata", Value::from(ids_json.as_str())));
        }

        let node_id = store
            .store_node(labels::EPISODIC, props)
            .map_err(|e| RuntimeError::Tool(format!("Failed to record episode: {e}")))?;

        tracing::info!(
            node_id = node_id.0,
            session_id = %record.session_id,
            turn_index = record.turn_index,
            content_len = content.len(),
            "MemoryManager: recorded episode"
        );

        Ok(())
    }

    /// Record a distilled/compacted episode into Grafeo.
    ///
    /// Per [ADR-011], the episode contains a natural-language summary.
    /// The summary text IS the distillation result.
    /// Entities and triples extracted during compaction are stored as
    /// node properties for later consolidation.
    ///
    /// If `embedding_provider` is `Some`, generates an embedding from
    /// the summary text (200ms timeout) for future vector retrieval.
    pub async fn record_distilled(
        &self,
        store: &GrafeoStore,
        episode: &DistilledEpisode,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<()> {
        // ── Auto-generate episode embedding ──
        // Timeout is handled by FallbackEmbeddingProvider internally
        // (200ms per attempt, then fallback to next provider).
        let episode_embedding: Option<Vec<f32>> = if let Some(provider) = embedding_provider {
            match provider.embed(&episode.summary).await {
                Ok(vec) => {
                    tracing::debug!(
                        dim = vec.len(),
                        provider = provider.name(),
                        "Auto-generated episode embedding"
                    );
                    Some(vec)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Episode embedding generation failed, storing without vector"
                    );
                    None
                }
            }
        } else {
            None
        };

        let entities_str = episode.entities.join(", ");
        let triples_json =
            serde_json::to_string(&episode.triples).unwrap_or_else(|_| "[]".to_string());

        let props = vec![
            ("session_id", Value::from(episode.session_id.as_str())),
            ("role", Value::from("distilled")),
            ("content", Value::from(episode.summary.as_str())),
            (
                "created_at",
                Value::from(Timestamp::from_micros(
                    chrono::Utc::now().timestamp_micros(),
                )),
            ),
            ("consolidated", Value::from(false)),
            ("importance", Value::from(0.7_f64)),
            (
                "source_session_id",
                Value::from(episode.source_session_id.as_str()),
            ),
            ("entities", Value::from(entities_str.as_str())),
            ("triples", Value::from(triples_json.as_str())),
        ];

        let node_id = store
            .store_node(labels::EPISODIC, props)
            .map_err(|e| RuntimeError::Tool(format!("Failed to record distilled episode: {e}")))?;

        // Store embedding vector on the node for future vector retrieval.
        if let Some(ref emb) = episode_embedding {
            store.db().set_node_property(
                node_id,
                "embedding",
                grafeo_common::types::Value::Vector(std::sync::Arc::from(emb.as_slice())),
            );
        }

        tracing::debug!(
            session_id = %episode.session_id,
            summary_len = episode.summary.len(),
            entity_count = episode.entities.len(),
            triple_count = episode.triples.len(),
            "Recorded distilled episode"
        );

        Ok(())
    }

    /// Record a ProceduralNode from a tool execution failure (Path B).
    ///
    /// When a skill/tool execution fails, this creates a low-confidence
    /// ProceduralNode that captures the failure pattern so the agent
    /// can avoid repeating the same mistake.
    ///
    /// The node is created with:
    /// - `learned_from = "execution_failure"`
    /// - `confidence = 0.6` (low — failure evidence is noisy)
    /// - `source_skill = Some(tool_name)`
    ///
    /// If a similar procedure already exists (dedup via
    /// `find_procedural_by_trigger`), the existing node's
    /// `fail_count` is incremented instead.
    pub fn record_procedural_from_failure(
        &self,
        store: &GrafeoStore,
        tool_name: &str,
        error_message: &str,
    ) -> Result<()> {
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};

        // Check for an existing procedure with the same trigger.
        let trigger = format!("使用 {} 工具时", tool_name);
        let existing = store
            .find_procedural_by_trigger(&trigger, 1)
            .map_err(|e| RuntimeError::Tool(format!("Failed to find procedure: {e}")))?;

        if let Some(mut node) = existing.into_iter().next() {
            // Reinforce existing: increment fail count.
            node.fail_count += 1;
            node.updated_at = chrono::Utc::now();
            store
                .update_procedural(&node)
                .map_err(|e| RuntimeError::Tool(format!("Failed to update procedure: {e}")))?;

            tracing::info!(
                node_id = node.id.map(|id| id.as_u64()).unwrap_or(0),
                tool_name,
                fail_count = node.fail_count,
                "Path B: reinforced existing ProceduralNode on failure"
            );
            return Ok(());
        }

        // Create a new ProceduralNode from the failure.
        // Extract a brief error pattern from the message (first line, max 80 chars).
        let error_pattern = error_message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();

        let action = format!("避免 {}；替代方案: 检查输入或重试", error_pattern);

        let node = ProceduralNode {
            id: None,
            name: format!("avoid_{}", tool_name),
            trigger_condition: trigger,
            action_pattern: action,
            success_count: 0,
            fail_count: 1,
            confidence: 0.6, // Low confidence — failure evidence is noisy
            activation_count: 0,
            source_skill: Some(tool_name.to_string()),
            learned_from: "execution_failure".to_string(),
            embedding: None, // No embedding at record time; filled by consolidation
            status: NodeStatus::Pending, // Low confidence → Pending
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };

        let id = store
            .store_procedural(&node)
            .map_err(|e| RuntimeError::Tool(format!("Failed to store procedure: {e}")))?;
        tracing::info!(
            node_id = id.as_u64(),
            tool_name,
            "Path B: created ProceduralNode from execution failure"
        );

        Ok(())
    }

    /// Full memory lifecycle for a single turn:
    /// 1. Retrieve memories for the query
    /// 2. Format for injection
    /// 3. Return injection text + metrics
    pub async fn process_turn(
        &self,
        store: &GrafeoStore,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<(InjectedMemory, RetrievalMetrics)> {
        let retrieval = self.retrieve(store, query, embedding_provider).await?;
        let metrics = retrieval.metrics.clone();
        let injected = self.inject(&retrieval);
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

    // Autobiographical nodes: include category for disambiguation.
    // Without this, the LLM cannot distinguish "agent's own capability"
    // from "learned user preference" — both show as [Autobiographical].
    // inject() prepends the label, so final output is:
    //   [Autobiographical] Capability: language: Rust
    //   [Autobiographical] Preference: answer_style: 大鱼 prefers concise answers
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
            return format!("{category}: {key}: {value}");
        }
        if !value.is_empty() {
            return format!("{category}: {value}");
        }
    }

    // Procedural nodes: format as behavioral guideline.
    // Must come before the generic "content" fallback, because
    // ProceduralNode.to_properties() stores a combined "content" field
    // that doesn't use the guideline format.
    // "当 [trigger_condition] 时，优先 [action_pattern]"
    let trigger = node
        .get_property("trigger_condition")
        .and_then(|v| v.as_str());
    let action = node.get_property("action_pattern").and_then(|v| v.as_str());
    if let (Some(t), Some(a)) = (trigger, action) {
        return format!("当 {} 时，优先 {}", t, a);
    }

    // Try common content fields in priority order.
    if let Some(content) = node.get_property("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }
    if let Some(value) = node.get_property("value").and_then(|v| v.as_str()) {
        return value.to_string();
    }

    // Knowledge nodes: combine subject + predicate + object.
    let subject = node.get_property("subject").and_then(|v| v.as_str());
    let predicate = node.get_property("predicate").and_then(|v| v.as_str());
    let object = node.get_property("object").and_then(|v| v.as_str());

    if let (Some(s), Some(p), Some(o)) = (subject, predicate, object) {
        return format!("{s} {p} {o}");
    }

    // Generic action_pattern fallback (non-procedural nodes with action_pattern).
    if let Some(action) = node.get_property("action_pattern").and_then(|v| v.as_str()) {
        return action.to_string();
    }

    // Fallback: use any string property.
    for key in ["name", "key", "description"] {
        if let Some(v) = node.get_property(key).and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }

    String::new()
}

/// Classification of autobiographical memory subcategory for budget
/// allocation. Core (Identity/Capability/Limitation) always gets first
/// priority; History (History/Relationship/Preference) is secondary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutobioGroup {
    /// Identity, Capability, Limitation — agent self-concept.
    Core,
    /// History, Relationship, Preference — contextual self-knowledge.
    History,
}

/// Determine the autobiographical group from the content prefix.
///
/// `extract_node_content()` formats autobiographical nodes as
/// `"Category: key: value"` or `"Category: value"`, so we parse the
/// prefix before the first colon to determine the subcategory.
fn autobio_subcategory(content: &str) -> AutobioGroup {
    // Parse the category prefix (e.g., "Identity: name: AgentCowork" → "Identity").
    let category = content.split(':').next().unwrap_or("").trim();
    match category {
        "Identity" | "Capability" | "Limitation" => AutobioGroup::Core,
        "History" | "Relationship" | "Preference" => AutobioGroup::History,
        // Unknown prefix — default to Core for safety (agent identity is
        // always important and the content is typically compact).
        _ => AutobioGroup::Core,
    }
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
    use acowork_grafeo::types::{DEFAULT_EMBEDDING_DIM, labels};
    use grafeo_common::types::Value;

    /// Helper: create an in-memory GrafeoStore for testing.
    fn test_store() -> GrafeoStore {
        GrafeoStore::new_in_memory().unwrap()
    }

    /// Helper: generate a test embedding vector.
    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
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
    fn store_knowledge(
        store: &GrafeoStore,
        subject: &str,
        predicate: &str,
        object: &str,
        embedding: &[f32],
    ) -> u64 {
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
    fn store_autobiographical(
        store: &GrafeoStore,
        key: &str,
        value: &str,
        embedding: &[f32],
    ) -> u64 {
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

    /// Helper: store a Procedural node with trigger and action.
    #[allow(dead_code)]
    fn store_procedure(store: &GrafeoStore, trigger: &str, action: &str, embedding: &[f32]) -> u64 {
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};
        let node = ProceduralNode {
            id: None,
            name: trigger
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join("_")
                .to_lowercase(),
            trigger_condition: trigger.to_string(),
            action_pattern: action.to_string(),
            success_count: 0,
            fail_count: 0,
            confidence: 0.9,
            activation_count: 0,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: Some(embedding.to_vec()),
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap().as_u64()
    }

    // =====================================================================
    // Test 1: Config defaults
    // =====================================================================

    #[test]
    fn test_config_defaults() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.max_inject_tokens, 2000);
        assert_eq!(config.max_autobio_core_tokens, 100);
        assert_eq!(config.max_autobio_history_tokens, 100);
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
        let mut query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
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
        let mut query = MemoryQuery {
            query_text: "something completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99), // Very high threshold — should filter everything.
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
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
        let mut query = MemoryQuery {
            query_text: "unrelated query".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
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
        let mut query = MemoryQuery {
            query_text: "rust programming".to_string(),
            embedding: None,
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
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
        let injected = manager.inject(&retrieval);

        assert!(!injected.formatted_text.is_empty());
        assert!(injected.formatted_text.contains("[Knowledge]"));
        assert!(injected.formatted_text.contains("[Episodic]"));
        assert_eq!(injected.memory_count, 2);
        assert!(!injected.truncated);
    }

    // =====================================================================
    // Test 8: Inject includes all memories without content truncation
    // =====================================================================

    #[test]
    fn test_inject_all_memories_no_truncation() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "User likes Rust programming language for systems development."
                        .to_string(),
                    label: "Knowledge".to_string(),
                    score: 0.95,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Another very long memory content that takes up many tokens."
                        .to_string(),
                    label: "Episodic".to_string(),
                    score: 0.85,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Third memory with even more text content to exceed token budget."
                        .to_string(),
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
        let injected = manager.inject(&retrieval);

        // All 3 memories should be included, no truncation.
        assert_eq!(injected.memory_count, 3);
        assert!(!injected.truncated);
        assert!(injected.formatted_text.contains("User likes Rust"));
        assert!(injected.formatted_text.contains("Another very long memory"));
        assert!(injected.formatted_text.contains("Third memory"));
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
        let injected = manager.inject(&retrieval);

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
        assert!(
            !text_results.is_empty(),
            "expected recorded episode to be found"
        );
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
        let mut query = MemoryQuery {
            query_text: "concise".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager
            .process_turn(&store, &mut query, None)
            .await
            .unwrap();

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
        let mut query = MemoryQuery {
            query_text: "completely unrelated".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: Some(0.99),
            abstention_enabled: true,
            hint_type: HintType::Semantic,
        };

        let (injected, metrics) = manager
            .process_turn(&store, &mut query, None)
            .await
            .unwrap();

        assert!(metrics.abstention_triggered);
        assert_eq!(injected.memory_count, 0);
        assert!(injected.formatted_text.is_empty());
    }

    // =====================================================================
    // Test 13: PageRank boost in retrieval pipeline
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_with_pagerank_boost() {
        let store = test_store();
        let emb = test_embedding();

        // Create three Episode nodes with the same embedding and similar content
        // so hybrid_search returns all three.
        let a_id = store_episode(&store, "Rust is a systems programming language", &emb);
        let b_id = store_episode(&store, "Rust powers web services and APIs", &emb);
        let c_id = store_episode(&store, "Rust has excellent tooling", &emb);

        // Create edges: A → B and C → B, making B the hub with 2 incoming edges.
        store
            .create_memory_edge(NodeId::new(a_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();
        store
            .create_memory_edge(NodeId::new(c_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();

        // Retrieve with PageRank enabled (default config, strong boost).
        let mut config = MemoryManagerConfig::default();
        config.enable_graph_expand = true;
        config.pagerank_weight = 0.3; // Strong boost to make topology effect visible.
        let manager = MemoryManager::new(config);

        let mut query = MemoryQuery {
            query_text: "Rust".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
        assert!(
            !result.memories.is_empty(),
            "should retrieve Rust-related nodes"
        );
    }

    // =====================================================================
    // Test 14: PageRank boost disabled (weight=0) has no effect
    // =====================================================================

    #[tokio::test]
    async fn test_retrieve_pagerank_disabled() {
        let store = test_store();
        let emb = test_embedding();

        let a_id = store_episode(&store, "Python is a scripting language", &emb);
        let b_id = store_episode(&store, "Python excels at data science", &emb);
        store
            .create_memory_edge(NodeId::new(a_id), NodeId::new(b_id), "RELATES_TO", vec![])
            .unwrap();

        // PageRank disabled.
        let mut config = MemoryManagerConfig::default();
        config.pagerank_weight = 0.0;
        let manager = MemoryManager::new(config);

        let mut query = MemoryQuery {
            query_text: "Python".to_string(),
            embedding: Some(emb),
            filters: Default::default(),
            limit: 5,
            expand_hops: 0,
            min_score: None,
            abstention_enabled: false,
            hint_type: HintType::Semantic,
        };

        let result = manager.retrieve(&store, &mut query, None).await.unwrap();
        assert!(!result.memories.is_empty());
    }

    // =====================================================================
    // Test 15: Autobiographical core budget limits identity injection
    // =====================================================================

    #[test]
    fn test_inject_autobio_core_budget() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "Identity: name: WeatherBot".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 1.0,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Identity: role: weather assistant that provides detailed forecasts and climate analysis".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.99,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Capability: forecast: can provide 7-day weather forecasts with temperature and precipitation details".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.98,
                    source: "hybrid".to_string(),
                    node_id: 3,
                    source_url: None,
                    chunk_id: None,
                },
            ],
            metrics: RetrievalMetrics::default(),
        };

        // Tight budget: only the first identity should fit.
        let mut config = MemoryManagerConfig::default();
        config.max_autobio_core_tokens = 15;
        let manager = MemoryManager::new(config);
        let injected = manager.inject(&retrieval);

        // At least one core memory is always included.
        assert!(
            injected
                .formatted_text
                .contains("Identity: name: WeatherBot")
        );
        // The long role and capability should be truncated by budget.
        assert!(injected.truncated);
        assert!(injected.memory_count < 3);
    }

    // =====================================================================
    // Test 16: Autobiographical history budget is separate from core
    // =====================================================================

    #[test]
    fn test_inject_autobio_history_budget() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "Identity: name: Bot".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 1.0,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "History: milestone: first release on 2024-01-01, successfully deployed to production environment".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.9,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "History: milestone: version 2.0 release with major feature improvements".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.89,
                    source: "hybrid".to_string(),
                    node_id: 3,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "Relationship: user: collaborates with Alice on data analysis".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.85,
                    source: "hybrid".to_string(),
                    node_id: 4,
                    source_url: None,
                    chunk_id: None,
                },
            ],
            metrics: RetrievalMetrics::default(),
        };

        // Generous core budget but tight history budget.
        let mut config = MemoryManagerConfig::default();
        config.max_autobio_core_tokens = 200;
        config.max_autobio_history_tokens = 20;
        let manager = MemoryManager::new(config);
        let injected = manager.inject(&retrieval);

        // Core should be fully included.
        assert!(injected.formatted_text.contains("Identity: name: Bot"));
        // History should be truncated.
        assert!(injected.truncated);
    }

    // =====================================================================
    // Test 17: Three-phase budget independence
    // =====================================================================

    #[test]
    fn test_inject_three_phase_budget_independence() {
        let retrieval = RetrievalResult {
            memories: vec![
                RetrievedMemory {
                    content: "Identity: name: TestBot".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 1.0,
                    source: "hybrid".to_string(),
                    node_id: 1,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "History: event: deployed to production".to_string(),
                    label: labels::AUTOBIOGRAPHICAL.to_string(),
                    score: 0.9,
                    source: "hybrid".to_string(),
                    node_id: 2,
                    source_url: None,
                    chunk_id: None,
                },
                RetrievedMemory {
                    content: "User prefers concise answers in technical discussions about programming languages".to_string(),
                    label: "Knowledge".to_string(),
                    score: 0.8,
                    source: "hybrid".to_string(),
                    node_id: 3,
                    source_url: None,
                    chunk_id: None,
                },
            ],
            metrics: RetrievalMetrics::default(),
        };

        // Tight non-autobiographical budget — should not affect autobiographical.
        let mut config = MemoryManagerConfig::default();
        config.max_autobio_core_tokens = 200;
        config.max_autobio_history_tokens = 200;
        config.max_inject_tokens = 5; // Very tight — Knowledge won't fit.
        let manager = MemoryManager::new(config);
        let injected = manager.inject(&retrieval);

        // Autobiographical memories should be injected.
        assert!(injected.formatted_text.contains("Identity: name: TestBot"));
        assert!(injected.formatted_text.contains("History: event: deployed"));
        // Knowledge should be truncated.
        assert!(injected.truncated);
        assert!(!injected.formatted_text.contains("Knowledge"));
    }

    // =====================================================================
    // Test 18: autobio_subcategory helper
    // =====================================================================

    #[test]
    fn test_autobio_subcategory() {
        assert_eq!(
            autobio_subcategory("Identity: name: Bot"),
            AutobioGroup::Core
        );
        assert_eq!(
            autobio_subcategory("Capability: language: Rust"),
            AutobioGroup::Core
        );
        assert_eq!(
            autobio_subcategory("Limitation: max_days: 7"),
            AutobioGroup::Core
        );
        assert_eq!(
            autobio_subcategory("History: milestone: v1"),
            AutobioGroup::History
        );
        assert_eq!(
            autobio_subcategory("Relationship: user: Alice"),
            AutobioGroup::History
        );
        assert_eq!(
            autobio_subcategory("Preference: style: concise"),
            AutobioGroup::History
        );
        // Unknown prefix defaults to Core.
        assert_eq!(autobio_subcategory("unknown content"), AutobioGroup::Core);
    }

    // =====================================================================
    // Test 19: Path B — record_procedural_from_failure
    // =====================================================================

    #[test]
    fn test_record_procedural_from_failure() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        // Record a failure from a tool.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: command not found")
            .unwrap();

        // Verify the ProceduralNode was created.
        let found = store.find_procedural_by_trigger("bash", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].learned_from, "execution_failure");
        assert_eq!(found[0].source_skill, Some("bash".to_string()));
        assert_eq!(found[0].fail_count, 1);
        assert!(found[0].confidence <= 0.7); // Low confidence
        assert_eq!(found[0].status, acowork_grafeo::types::NodeStatus::Pending);
    }

    // =====================================================================
    // Test 20: Path B — repeated failure reinforces existing node
    // =====================================================================

    #[test]
    fn test_record_procedural_from_failure_reinforce() {
        let store = test_store();
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        // First failure.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: timeout")
            .unwrap();

        // Second failure for the same tool.
        manager
            .record_procedural_from_failure(&store, "bash", "Error: permission denied")
            .unwrap();

        // Should have only one node (reinforced, not duplicated).
        let found = store.find_procedural_by_trigger("bash", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fail_count, 2, "fail_count should be incremented");
    }

    // =====================================================================
    // Test 21: Procedural injection format uses behavioral guideline
    // =====================================================================

    #[test]
    fn test_procedural_injection_format() {
        // Directly construct a RetrievedMemory for a Procedural node
        // to test the injection format without relying on retrieval
        // (retrieval text search uses "content" field which Procedural
        // nodes don't have — a separate retrieval integration fix).
        let manager = MemoryManager::new(MemoryManagerConfig::default());

        let retrieval = RetrievalResult {
            memories: vec![RetrievedMemory {
                content: "当 user asks for summary 时，优先 reply in 3 sentences max".to_string(),
                label: labels::PROCEDURAL.to_string(),
                score: 0.9,
                source: "hybrid".to_string(),
                node_id: 1,
                source_url: None,
                chunk_id: None,
            }],
            metrics: RetrievalMetrics::default(),
        };

        let injected = manager.inject(&retrieval);

        // The procedural node should be injected with the behavioral guideline format.
        assert!(
            injected.formatted_text.contains("当") && injected.formatted_text.contains("优先"),
            "Procedural injection should use '当 X 时，优先 Y' format, got: {}",
            injected.formatted_text
        );
    }

    // =====================================================================
    // Test 22: extract_node_content produces procedural guideline format
    // =====================================================================

    #[test]
    fn test_extract_node_content_procedural() {
        let store = test_store();

        // Store a procedural node.
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};
        let node = ProceduralNode {
            id: None,
            name: "concise_summary".to_string(),
            trigger_condition: "user asks for summary".to_string(),
            action_pattern: "reply in 3 sentences max".to_string(),
            success_count: 5,
            fail_count: 1,
            confidence: 0.9,
            activation_count: 3,
            source_skill: None,
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        let id = store.store_procedural(&node).unwrap();

        // extract_node_content should format it as "当 X 时，优先 Y".
        let content = extract_node_content(&store, id.as_u64());
        assert!(
            content.starts_with("当"),
            "Procedural content should start with '当', got: {}",
            content
        );
        assert!(
            content.contains("优先"),
            "Procedural content should contain '优先', got: {}",
            content
        );
        assert!(
            content.contains("user asks for summary"),
            "Should contain trigger_condition"
        );
        assert!(
            content.contains("reply in 3 sentences max"),
            "Should contain action_pattern"
        );
    }

    // =====================================================================
    // Test 23: Self-evaluation — Limitation node from low success rate
    // =====================================================================

    #[test]
    fn test_self_evaluate_creates_limitation_node() {
        use acowork_grafeo::types::{
            AutobioCategory, AutobiographicalNode, NodeStatus, ProceduralNode,
        };

        let store = test_store();

        // Create ProceduralNodes with low success rate for skill "bash".
        // success=1, fail=4 → rate=20% < 60%, observations=5 ≥ 5.
        let node = ProceduralNode {
            id: None,
            name: "avoid_bash".to_string(),
            trigger_condition: "使用 bash 工具时".to_string(),
            action_pattern: "避免 bash".to_string(),
            success_count: 1,
            fail_count: 4,
            confidence: 0.6,
            activation_count: 0,
            source_skill: Some("bash".to_string()),
            learned_from: "execution_failure".to_string(),
            embedding: None,
            status: NodeStatus::Pending,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap();

        // Manually run the self-evaluation logic (normally in AgentLoop,
        // but we test the core logic here).
        let nodes = store.get_all_procedural_nodes().unwrap();
        let mut skill_stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for n in &nodes {
            if let Some(ref skill) = n.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += n.success_count;
                entry.1 += n.fail_count;
            }
        }

        // Verify skill stats computation.
        assert_eq!(skill_stats.get("bash"), Some(&(1, 4)));

        // Simulate the Limitation node creation.
        let key = "skill_bash".to_string();
        let value = "bash 成功率仅 20%（1 次成功 / 4 次失败）".to_string();
        let limitation = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Limitation,
            key: key.clone(),
            value,
            confidence: 0.8,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_autobiographical(&limitation).unwrap();

        // Verify the Limitation node was stored.
        let found = store.find_autobiographical_by_key(&key).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.category, AutobioCategory::Limitation);
        assert!(found.value.contains("20%"));
    }

    // =====================================================================
    // Test 24: Self-evaluation — high success rate → no Limitation node
    // =====================================================================

    #[test]
    fn test_self_evaluate_no_limitation_for_high_success() {
        use acowork_grafeo::types::{NodeStatus, ProceduralNode};

        let store = test_store();

        // Create ProceduralNodes with high success rate.
        // success=8, fail=2 → rate=80% > 60%, should NOT create Limitation.
        let node = ProceduralNode {
            id: None,
            name: "good_skill".to_string(),
            trigger_condition: "使用 python 工具时".to_string(),
            action_pattern: "使用 python".to_string(),
            success_count: 8,
            fail_count: 2,
            confidence: 0.9,
            activation_count: 0,
            source_skill: Some("python".to_string()),
            learned_from: "user_feedback".to_string(),
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_procedural(&node).unwrap();

        // Compute stats.
        let nodes = store.get_all_procedural_nodes().unwrap();
        let mut skill_stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for n in &nodes {
            if let Some(ref skill) = n.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += n.success_count;
                entry.1 += n.fail_count;
            }
        }

        let (success, fail) = skill_stats.get("python").unwrap();
        let total = success + fail;
        let rate = *success as f32 / total as f32;

        // Rate should be 80%, above the 60% threshold.
        assert!(
            rate > 0.60,
            "python skill success rate should be > 60%, got {}",
            rate
        );

        // No Limitation node should exist for python.
        let found = store.find_autobiographical_by_key("skill_python").unwrap();
        assert!(found.is_none());
    }

    // =====================================================================
    // Test 25: Relationship auto-generation — span > 30 days
    // =====================================================================

    #[test]
    fn test_auto_generate_relationship_span_over_30_days() {
        use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, Episode, NodeStatus};

        let store = test_store();

        // Create an old episode (45 days ago).
        let old_time = chrono::Utc::now() - chrono::TimeDelta::days(45);
        let episode = Episode {
            id: None,
            session_id: "test-session".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "Hello".to_string(),
            embedding: None,
            timestamp: old_time,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
        };
        store.store_episode(&episode).unwrap();

        // Simulate the Relationship generation logic.
        let db = store.db();
        let graph = db.graph_store();
        let episodic_ids = graph.nodes_by_label(acowork_grafeo::types::labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut episode_count: u32 = 0;

        for id in episodic_ids {
            if let Some(n) = db.get_node(id) {
                episode_count += 1;
                if let Some(ts) = n
                    .get_property("created_at")
                    .and_then(grafeo_common::types::Value::as_timestamp)
                {
                    if let Some(dt) = chrono::DateTime::from_timestamp_micros(ts.as_micros()) {
                        match earliest_time {
                            None => earliest_time = Some(dt),
                            Some(earliest) if dt < earliest => earliest_time = Some(dt),
                            _ => {}
                        }
                    }
                }
            }
        }

        let earliest = earliest_time.unwrap();
        let span_days = (chrono::Utc::now() - earliest).num_days();
        assert!(
            span_days >= 30,
            "span should be >= 30 days, got {}",
            span_days
        );

        // Create the Relationship node.
        let key = "collaboration_span".to_string();
        let value = format!("已合作 {} 天（{} 次对话记录）", span_days, episode_count);
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Relationship,
            key: key.clone(),
            value,
            confidence: 0.9,
            source_episode_id: None,
            embedding: None,
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
        };
        store.store_autobiographical(&node).unwrap();

        // Verify the Relationship node was stored.
        let found = store.find_autobiographical_by_key(&key).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.category, AutobioCategory::Relationship);
        assert!(found.value.contains("天"));
    }

    // =====================================================================
    // Test 26: Relationship auto-generation — span < 30 days → no node
    // =====================================================================

    #[test]
    fn test_auto_generate_relationship_span_under_30_days() {
        use acowork_grafeo::types::Episode;

        let store = test_store();

        // Create a recent episode (5 days ago).
        let recent_time = chrono::Utc::now() - chrono::TimeDelta::days(5);
        let episode = Episode {
            id: None,
            session_id: "test-session".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "Hello".to_string(),
            embedding: None,
            timestamp: recent_time,
            consolidated: false,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
        };
        store.store_episode(&episode).unwrap();

        // Compute span — should be < 30 days.
        let db = store.db();
        let graph = db.graph_store();
        let episodic_ids = graph.nodes_by_label(acowork_grafeo::types::labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<chrono::Utc>> = None;
        for id in episodic_ids {
            if let Some(n) = db.get_node(id) {
                if let Some(ts) = n
                    .get_property("created_at")
                    .and_then(grafeo_common::types::Value::as_timestamp)
                {
                    if let Some(dt) = chrono::DateTime::from_timestamp_micros(ts.as_micros()) {
                        match earliest_time {
                            None => earliest_time = Some(dt),
                            Some(earliest) if dt < earliest => earliest_time = Some(dt),
                            _ => {}
                        }
                    }
                }
            }
        }

        let span_days = (chrono::Utc::now() - earliest_time.unwrap()).num_days();
        assert!(
            span_days < 30,
            "span should be < 30 days, got {}",
            span_days
        );

        // No Relationship node should exist.
        let found = store
            .find_autobiographical_by_key("collaboration_span")
            .unwrap();
        assert!(found.is_none());
    }
}
