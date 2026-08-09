//! MemoryManager - orchestrates the three-phase memory lifecycle.
//!
//! 1. Retrieve - search relevant memories before LLM generation
//! 2. Inject  - format and inject memories into the system prompt
//! 3. Record  - asynchronously record the conversation episode
//!
//! ADR-051 P2: Moved from acowork-runtime to acowork-memory.
//! Error type changed from RuntimeError::Tool to AcoworkError::Memory.
//! EmbeddingProvider trait imported from acowork-core.
use std::collections::HashMap;

use crate::{
    labels, Episode, HintType, MemoryProvider, MemoryQuery, RetrievalMetrics,
};
use chrono::{DateTime, Utc};

use acowork_core::EmbeddingProvider;
use crate::types::DistilledEpisode;
use acowork_core::error::{AcoworkError, Result};

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
/// ADR-051 C4: RAG channel removed from MemoryManager.
/// RAG retrieval is now handled by `loop_memory.rs` which calls
/// `rag_provider.query()` separately and merges results by score.
pub struct MemoryManager {
    config: MemoryManagerConfig,
}

impl MemoryManager {
    /// Create a new MemoryManager with the given configuration.
    pub fn new(config: MemoryManagerConfig) -> Self {
        Self { config }
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
    /// + RAG channel (if rag_provider is Some, run in parallel).
    ///
    /// RAG channel uses the user message as query with default top_k=3.
    /// Results from both channels are merged and sorted by score.
    /// Source annotations distinguish [Grafeo] vs [RAG:<tool_name>].
    pub async fn retrieve(
        &self,
        provider: &dyn MemoryProvider,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<RetrievalResult> {
        // ── Auto-generate embedding if needed ──
        // Timeout is handled by FallbackEmbeddingProvider internally
        // (200ms per attempt, then fallback to next provider).
        if query.embedding.is_none()
            && let Some(emb_prov) = embedding_provider {
                match emb_prov.embed(&query.query_text).await {
                    Ok(vec) => {
                        tracing::debug!(
                            dim = vec.len(),
                            provider = emb_prov.name(),
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

        let k = if query.limit > 0 {
            query.limit
        } else {
            self.config.default_k
        };
        let min_score = query.min_score.unwrap_or(self.config.default_min_score);
        let hint_type = query.hint_type;
        let (vector_weight, text_weight, _graph_weight) = hint_weights(hint_type);

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
                provider
                    .hybrid_search_full(
                        label,
                        &query.query_text,
                        embedding,
                        k,
                        text_weight,
                        vector_weight,
                        Some(min_score),
                    )
                    .map_err(|e| AcoworkError::Memory(format!("Hybrid search failed: {e}")))
            } else {
                // Fallback to text search when no embedding is available.
                provider
                    .text_search_with_filter(
                        label,
                        "content",
                        &query.query_text,
                        k,
                        Some(min_score),
                    )
                    .map_err(|e| AcoworkError::Memory(format!("Text search failed: {e}")))
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
                        all_results.push((node_id, score, label.to_string(), source));
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
            let seeds: Vec<(u64, f64)> = all_results
                .iter()
                .map(|(id, score, _, _)| (*id, *score))
                .collect();

            match provider
                .graph_expand_seeded(&seeds, hint_type.as_str())
                .map_err(|e| AcoworkError::Memory(format!("Graph expand failed: {e}")))
            {
                Ok(expanded) => {
                    graph_expand_count = expanded.len();
                    for (node_id, score, label) in expanded {
                        all_results.push((node_id, score, label, "graph".to_string()));
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
                match provider.get_node_session_id(*node_id) {
                    Ok(Some(sid)) => sid != *exclude_sid,
                    _ => true, // No session_id or error -> keep
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
            let mut scored: Vec<(u64, f64)> = best_by_id
                .iter()
                .map(|(id, (score, _, _))| (*id, *score))
                .collect();

            if let Err(e) = provider.apply_pagerank_boost(&mut scored, self.config.pagerank_weight) {
                tracing::warn!("PageRank boost failed, continuing with unboosted scores: {e}");
            } else {
                // Map boosted scores back to best_by_id.
                for (node_id, boosted_score) in scored {
                    if let Some(entry) = best_by_id.get_mut(&node_id) {
                        entry.0 = boosted_score;
                    }
                }
            }
        }

        // Build RetrievedMemory list, sorted by score descending.
        let mut memories: Vec<RetrievedMemory> = Vec::new();
        for (node_id, (score, label, source)) in best_by_id {
            let content = match provider.get_node_content(node_id) {
                Ok(Some(c)) => c,
                Ok(None) => String::new(),
                Err(e) => {
                    tracing::warn!(node_id, error = %e, "Failed to extract node content");
                    String::new()
                }
            };
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
            "Retrieved {} memories (max_score={:.3}, avg_score={:.3}, graph_expanded={})",
            result_count,
            max_score,
            avg_score,
            graph_expand_count,
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
        provider: &dyn MemoryProvider,
    ) -> InjectedMemory {
        let mut injected = self.inject(retrieval);

        // Check for pending ambiguous conflicts.
        if let Ok(true) = provider.should_trigger_confirmation()
            && let Ok(Some(hint)) = provider.generate_confirmation_hint() {
                let hint_line = format!("[Ambiguous] {}", hint);
                let hint_tokens = estimate_tokens(&hint_line);
                injected.formatted_text = format!("{}\n{}", injected.formatted_text, hint_line);
                injected.token_count += hint_tokens;
                injected.memory_count += 1;
            }

        injected
    }

    /// Record a conversation turn as an episode.
    ///
    /// In production this runs asynchronously; for now synchronous.
    pub fn record(
        &self,
        provider: &dyn MemoryProvider,
        record: &ConversationRecord,
    ) -> Result<()> {
        let content = format!(
            "User: {}\nAssistant: {}",
            record.user_message, record.assistant_response
        );

        let mut metadata = HashMap::new();
        if !record.retrieved_memory_ids.is_empty() {
            metadata.insert(
                "retrieved_memory_ids".to_string(),
                serde_json::to_value(&record.retrieved_memory_ids)
                    .map_err(AcoworkError::Serialization)?,
            );
        }

        let episode = Episode {
            session_id: record.session_id.clone(),
            turn_index: record.turn_index,
            role: "conversation".to_string(),
            content,
            embedding: None,
            timestamp: record.timestamp,
            consolidated: false,
            metadata,
            importance: 0.5,
        };

        provider
            .store_episode(&episode)
            .map_err(|e| AcoworkError::Memory(format!("Failed to record episode: {e}")))?;

        tracing::info!(
            session_id = %record.session_id,
            turn_index = record.turn_index,
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
        provider: &dyn MemoryProvider,
        episode: &DistilledEpisode,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<()> {
        // Auto-generate episode embedding (200ms timeout via FallbackEmbeddingProvider).
        let episode_embedding: Option<Vec<f32>> = if let Some(emb_prov) = embedding_provider {
            match emb_prov.embed(&episode.summary).await {
                Ok(vec) => {
                    tracing::debug!(
                        dim = vec.len(),
                        provider = emb_prov.name(),
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

        let mut metadata = HashMap::new();
        metadata.insert(
            "source_session_id".to_string(),
            serde_json::Value::String(episode.source_session_id.clone()),
        );
        metadata.insert(
            "entities".to_string(),
            serde_json::Value::String(entities_str),
        );
        metadata.insert(
            "triples".to_string(),
            serde_json::Value::String(triples_json),
        );

        let ep = Episode {
            session_id: episode.session_id.clone(),
            turn_index: 0,
            role: "distilled".to_string(),
            content: episode.summary.clone(),
            embedding: episode_embedding,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata,
            importance: 0.7,
        };

        provider
            .store_episode(&ep)
            .map_err(|e| AcoworkError::Memory(format!("Failed to record distilled episode: {e}")))?;

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
        provider: &dyn MemoryProvider,
        tool_name: &str,
        error_message: &str,
    ) -> Result<()> {
        use crate::{NodeStatus, ProceduralNode};

        // Check for an existing procedure with the same trigger.
        let trigger = format!("使用 {} 工具时", tool_name);
        let existing = provider
            .find_procedural_by_trigger(&trigger, 1)
            .map_err(|e| AcoworkError::Memory(format!("Failed to find procedure: {e}")))?;

        if let Some(mut node) = existing.into_iter().next() {
            // Reinforce existing: increment fail count.
            node.fail_count += 1;
            node.updated_at = chrono::Utc::now();
            provider
                .update_procedural(&node)
                .map_err(|e| AcoworkError::Memory(format!("Failed to update procedure: {e}")))?;

            tracing::info!(
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

        provider
            .store_procedural(&node)
            .map_err(|e| AcoworkError::Memory(format!("Failed to store procedure: {e}")))?;
        tracing::info!(
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
        provider: &dyn MemoryProvider,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<(InjectedMemory, RetrievalMetrics)> {
        let retrieval = self.retrieve(provider, query, embedding_provider).await?;
        let metrics = retrieval.metrics.clone();
        let injected = self.inject(&retrieval);
        Ok((injected, metrics))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get hybrid search weights based on hint type.
///
/// Returns `(vector_weight, text_weight, graph_weight)`.
/// Inlined from `acowork_grafeo::spreading::get_hint_weights` (ADR-051 C4).
fn hint_weights(hint_type: HintType) -> (f64, f64, f64) {
    match hint_type {
        HintType::Semantic => (0.8, 0.2, 0.0),
        HintType::Factual => (0.5, 0.5, 0.0),
        HintType::Relational => (0.6, 0.2, 0.2),
        HintType::Identity => (0.3, 0.7, 0.0),
    }
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
    // Parse the category prefix (e.g., "Identity: name: ACowork" → "Identity").
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
// ---------------------------------------------------------------------------
// Tests (pure logic - no GrafeoStore dependency)
// GrafeoStore-dependent integration tests remain in acowork-runtime.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_manager_new() {
        let config = MemoryManagerConfig::default();
        let manager = MemoryManager::new(config.clone());
        assert_eq!(manager.config.max_inject_tokens, config.max_inject_tokens);
    }

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

    #[test]
    #[allow(clippy::field_reassign_with_default)]
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

    #[test]
    #[allow(clippy::field_reassign_with_default)]
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

    #[test]
    #[allow(clippy::field_reassign_with_default)]
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

}
