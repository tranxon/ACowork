//! MemoryManager - orchestrates the three-phase memory lifecycle.
//!
//! 1. Retrieve - search relevant memories before LLM generation
//! 2. Inject  - format and inject memories into the system prompt
//! 3. Record  - persist distilled episodes (via `record_distilled`)
//!
//! Conversation turns are NOT automatically recorded as episodic memory
//! (the former `record_turn` / `ConversationRecord` channel was removed —
//! dead code, see docs/memory-write-entrypoints.md).
//!
//! ADR-051 P2: Moved from acowork-runtime to acowork-memory.
//! Error type changed from RuntimeError::Tool to AcoworkError::Memory.
//! EmbeddingProvider trait imported from acowork-core.
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::{
    labels, Episode, HintType, MemoryProvider, MemoryQuery, RetrievalMetrics,
};
use crate::consolidation::{EmbeddingFn, GeneralizationConfig};
use crate::quality::MemoryQualityConfig;

use acowork_core::EmbeddingProvider;
use crate::types::{DistilledEpisode, NodeStatus};
use acowork_core::error::{AcoworkError, Result};

// ---------------------------------------------------------------------------
// Procedural embedding fallback (ADR-057 P0 ProceduralNode embedding required)
// ---------------------------------------------------------------------------
//
// All three ProceduralNode creation paths must supply a non-None embedding.
// When a real `EmbeddingFn` is available we use it; otherwise we fall back to a
// deterministic, dependency-free hash embedding of dimension 384 (matches
// `HnswConfig::default().vector_dim`) so the node remains indexable from the
// moment it is written.
//
// This is intentionally a pure function with no external dependencies so it
// can live in `acowork-memory` (upstream of `acowork-grafeo`).

/// Default embedding dimension. Must match `HnswConfig::default().vector_dim`.
pub const PROCEDURAL_FALLBACK_DIM: usize = 384;

/// Default abstention guidance prompt (G9).
///
/// Mirrors `AbstentionConfig::default().abstention_prompt` in `acowork-grafeo`.
/// Kept as a constant here so `acowork-memory` (upstream) never imports the
/// grafeo crate; `MemoryManagerConfig.abstention_prompt` overrides it.
pub const DEFAULT_ABSTENTION_PROMPT: &str = "When you are not confident about the \
    information from memory, respond with 'I'm not sure about this' rather than guessing.";

/// Deterministic, dependency-free fallback embedding for procedural nodes when
/// no `EmbeddingFn` is available. The result is not semantically meaningful,
/// but it is stable, non-zero, and dimensionally compatible with the HNSW
/// index so the stored `ProceduralNode` is immediately vector-searchable.
pub fn procedural_embedding_fallback(text: &str) -> Vec<f32> {
    let mut out = vec![0.0f32; PROCEDURAL_FALLBACK_DIM];
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let h1 = hasher.finish();
    for (i, slot) in out.iter_mut().enumerate() {
        let mut h = DefaultHasher::new();
        (h1.wrapping_add(i as u64)).hash(&mut h);
        let v = h.finish();
        *slot = ((v % 2000) as f32 - 1000.0) / 1000.0;
    }
    out
}

/// Resolve the procedural embedding using the provided `EmbeddingFn` or fall
/// back to [`procedural_embedding_fallback`]. Never returns `None` — the
/// `embedding` field on a persisted `ProceduralNode` must be `Some(_)` per
/// ADR-057 P0.
pub fn procedural_embedding_for(
    text: &str,
    embedding_fn: Option<&EmbeddingFn>,
) -> Vec<f32> {
    match embedding_fn {
        Some(f) => f(text),
        None => procedural_embedding_fallback(text),
    }
}

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
    /// Centralized memory quality parameters (ADR-062 D2).
    ///
    /// Source of truth for the retrieval-side quality knobs that were
    /// previously separate `MemoryManagerConfig` fields:
    /// - `quality.min_score` (replaces the old `default_min_score`)
    /// - `quality.pagerank_weight` (replaces the old `pagerank_weight`)
    /// - `quality.exclude_dormant` (Dormant retrieval exclusion, D1)
    ///
    /// The write-path thresholds (dedup / consolidation / graph expansion /
    /// edge weight) are pushed down to the `MemoryProvider` via
    /// `MemoryProvider::apply_quality_config` so engine internals read from
    /// the same config (ADR-062 §4.1). `Default` mirrors current behaviour
    /// ("zero configuration = current behaviour").
    pub quality: MemoryQualityConfig,
    /// Abstention guidance prompt injected when retrieval returns nothing
    /// and `query.abstention_enabled` is true (G9).
    ///
    /// When `None` (default), the built-in default text is used — mirroring
    /// `AbstentionConfig::default().abstention_prompt` in `acowork-grafeo`.
    /// This layer must NOT import `acowork-grafeo` (dependency direction),
    /// hence the constant lives here.
    pub abstention_prompt: Option<String>,
    /// Enable graph expansion (default: true).
    pub enable_graph_expand: bool,
    /// Record episodes asynchronously (default: true).
    pub record_async: bool,
    /// Per-turn auto-injection of retrieved memories (default: **true**,
    /// ADR-062 M5 — first-turn trigger per session, ADR-060 §6.3).
    ///
    /// When enabled, the agent loop calls `MemoryManager::retrieve_and_inject`
    /// at most ONCE per session, on the session's FIRST user message
    /// (`AgentLoop.memory_retrieved_for_session` guards later turns).
    ///
    /// When disabled, auto-injection is NOT called — the LLM can still
    /// trigger explicit deep recall via the `memory_recall` tool
    /// (`MemoryQuery::deep_recall`).
    ///
    /// **History**: off by default since 2026-09-12 (unmature memory layer,
    /// low-precision raw-message queries). Temporarily re-opened by ADR-062
    /// M5 once the P2 benchmark cleared the §5.2 gates (Dormant exclusion +
    /// min_score fix + keyword quality gate), then reverted to OFF by
    /// default: with auto-inject ON, the first-turn injection duplicates
    /// what the LLM already retrieves via an explicit `memory_recall` call
    /// (both paths query on the same user message). The LLM is now relied
    /// upon for explicit recall; auto-inject is a per-agent opt-in.
    ///
    /// **Per-agent opt-in**: an agent can enable it via the manifest
    /// `[memory.quality].auto_inject_enabled = true` (ADR-062 §6.1).
    pub auto_inject_enabled: bool,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            max_inject_tokens: 2000,
            max_autobio_core_tokens: 100,
            max_autobio_history_tokens: 100,
            default_k: 10,
            quality: MemoryQualityConfig::default(),
            abstention_prompt: None,
            enable_graph_expand: true,
            record_async: true,
            auto_inject_enabled: false,
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
    /// Abstention guidance prompt to inject into the system prompt when
    /// abstention triggered (empty result set + `abstention_enabled`).
    /// `None` when abstention was not triggered or is disabled.
    pub abstention_prompt: Option<String>,
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

/// Result of `retrieve_and_inject()` - the combined retrieve+inject+activate
/// operation. ADR-051 P3.
#[derive(Debug)]
pub struct RetrieveAndInjectResult {
    /// Injected memory text ready for ContextBuilder.
    pub injected: InjectedMemory,
    /// Retrieval metrics for monitoring.
    pub metrics: RetrievalMetrics,
    /// Node IDs of retrieved memories (for traceability/debugging).
    pub memory_ids: Vec<String>,
    /// Pending ambiguous conflict hint, if any.
    pub ambiguous_hint: Option<String>,
    /// Abstention guidance prompt to inject into the system prompt when
    /// abstention triggered (G9). `None` otherwise.
    pub abstention_prompt: Option<String>,
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

    /// Read the manager's configuration (e.g. to check feature switches).
    pub fn config(&self) -> &MemoryManagerConfig {
        &self.config
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
        let min_score = query.min_score.unwrap_or(self.config.quality.min_score);
        let hint_type = query.hint_type;
        let (vector_weight, text_weight, _graph_weight) = hint_weights(hint_type);

        // G10 (design §6.6): search ALL 4 labels regardless of hint type.
        // The design explicitly states that "searching all 4 labels is the
        // safer choice" — even Identity queries (e.g. `auto_inject`) must
        // reach Knowledge / Procedural layers. Narrowing per hint type was
        // removed (would require an explicit design decision to re-add).
        let search_labels: Vec<&str> = vec![
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ];

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

        // ADR-062 D1: Exclude Dormant nodes from the final result set.
        // Dormant nodes are decayed below threshold but retained (design
        // §5.2) — they may still act as graph-expansion seeds (kept in
        // `all_results` during expansion above) but must NOT appear in
        // retrieval results. Filtering here (before dedup / best_by_id)
        // preserves graph bridging.
        //
        // Pending nodes intentionally remain retrievable: they are
        // low-confidence but searchable, and are naturally down-ranked by
        // confidence in the final ordering.
        if self.config.quality.exclude_dormant && !all_results.is_empty() {
            let before = all_results.len();
            // Resolve status once per unique node to avoid repeated lookups.
            let mut status_cache: HashMap<u64, Option<NodeStatus>> = HashMap::new();
            for (id, _, _, _) in &all_results {
                status_cache
                    .entry(*id)
                    .or_insert_with(|| provider.get_node_status(*id).ok().flatten());
            }
            all_results.retain(|(id, _, _, _)| {
                status_cache
                    .get(id)
                    .and_then(|s| s.clone())
                    .is_none_or(|s| s != NodeStatus::Dormant)
            });
            tracing::debug!(
                before,
                after = all_results.len(),
                "Excluded Dormant nodes from retrieval results"
            );
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

        // Post-filter: apply the time-range window (created_at in [since, until]).
        // Drives the `memory_recall` tool's `since`/`until` parameters, which
        // were previously validated but never applied (ADR-062 M5).
        if let Some((since, until)) = query.filters.time_range {
            let before = best_by_id.len();
            best_by_id.retain(|node_id, _| {
                match provider.get_node_created_at(*node_id) {
                    Ok(Some(ts)) => ts >= since && ts <= until,
                    // No timestamp or error -> keep (defensive; mirrors the
                    // exclude_session_id filter's keep-on-unknown policy).
                    _ => true,
                }
            });
            tracing::debug!(
                before,
                after = best_by_id.len(),
                since = %since,
                until = %until,
                "Applied time-range filter to retrieval"
            );
        }

        // Apply PageRank topology boost for re-ranking (S2.8.3).
        // Only when graph expansion is enabled and weight > 0.
        if self.config.enable_graph_expand
            && self.config.quality.pagerank_weight > 0.0
            && !best_by_id.is_empty()
        {
            let mut scored: Vec<(u64, f64)> = best_by_id
                .iter()
                .map(|(id, (score, _, _))| (*id, *score))
                .collect();

            if let Err(e) =
                provider.apply_pagerank_boost(&mut scored, self.config.quality.pagerank_weight)
            {
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

        // G9: When abstention triggers, attach the abstention guidance prompt
        // so the caller can inject it into the system prompt. This prevents
        // the LLM from fabricating answers based on an empty result set.
        // The prompt text comes from `MemoryManagerConfig.abstention_prompt`
        // (defaults to the built-in constant, mirroring grafeo's
        // `AbstentionConfig::default().abstention_prompt`).
        let abstention_prompt = if abstention_triggered {
            Some(
                self.config
                    .abstention_prompt
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ABSTENTION_PROMPT.to_string()),
            )
        } else {
            None
        };

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

        Ok(RetrievalResult {
            memories,
            metrics,
            abstention_prompt,
        })
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

    /// Record a distilled/compacted episode into the episodic layer.
    ///
    /// ADR-057 → triples removal (this PR): only the natural-language
    /// summary is persisted. Triple-based knowledge extraction was removed
    /// because compact-model output quality was too low (see
    /// [`parse_compact_output_strict`]’s gate rationale). Knowledge-layer
    /// updates flow through the `memory_store` tool / procedural creation
    /// paths instead (see `docs/memory-write-entrypoints.md`).
    ///
    /// Embedding of the summary is best-effort (D1 — failure degrades to
    /// `None`, episode still stored, vector recall drops for that node).
    pub async fn record_distilled(
        &self,
        provider: &dyn MemoryProvider,
        episode: &DistilledEpisode,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<()> {
        let summary_embedding = match embedding_provider {
            Some(prov) => prov.embed(&episode.summary).await.ok(),
            None => None,
        };

        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "source_session_id".to_string(),
            serde_json::Value::String(episode.source_session_id.clone()),
        );

        let ep = Episode {
            session_id: episode.session_id.clone(),
            turn_index: 0,
            role: "distilled".to_string(),
            content: episode.summary.clone(),
            embedding: summary_embedding,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata,
            importance: 0.7,
        };
        provider.store_episode(&ep)?;

        tracing::debug!(
            session_id = %episode.session_id,
            summary_len = episode.summary.len(),
            "Recorded distilled episode (summary-only, no triple landing)"
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

    // ── ADR-051 P3: High-level semantic methods ─────────────────────────
    //
    // These methods encapsulate all direct provider CRUD operations.
    // loop_memory.rs should only call these, never raw provider methods.

    /// Retrieve memories, inject them, activate procedural nodes, and check
    /// for ambiguous conflicts - all in one call.
    ///
    /// This is the primary entry point for per-turn memory injection.
    /// The caller receives the formatted text, metrics, memory IDs for
    /// traceability, and an optional ambiguous conflict hint to inject
    /// into the context.
    ///
    /// ADR-051 P3: Replaces the inline retrieve+inject+activate+ambiguity
    /// logic that was in loop_memory.rs.
    pub async fn retrieve_and_inject(
        &self,
        provider: &dyn MemoryProvider,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<RetrieveAndInjectResult> {
        let retrieval = self.retrieve(provider, query, embedding_provider).await?;
        let metrics = retrieval.metrics.clone();

        // Capture node IDs before inject (for traceability).
        let memory_ids: Vec<String> = retrieval
            .memories
            .iter()
            .filter(|m| m.node_id != 0)
            .map(|m| m.node_id.to_string())
            .collect();

        // Activate ProceduralNodes that were retrieved.
        self.activate_procedural_nodes(provider, &retrieval.memories);

        // Inject with ambiguous conflict hints.
        let injected = self.inject_with_ambiguous_hints(&retrieval, provider);

        // Extract ambiguous hint separately for the caller.
        let ambiguous_hint = if injected.formatted_text.contains("[Ambiguous]") {
            // Extract the hint text after the last "[Ambiguous] " marker.
            injected
                .formatted_text
                .rsplit_once("[Ambiguous] ")
                .map(|(_, hint)| hint.to_string())
        } else {
            None
        };

        Ok(RetrieveAndInjectResult {
            injected,
            metrics,
            memory_ids,
            ambiguous_hint,
            abstention_prompt: retrieval.abstention_prompt,
        })
    }

    /// Run all post-compaction maintenance tasks.
    ///
    /// Executes in sequence:
    /// 1. Experience generalization (Path C) - extract behavior patterns
    /// 2. History compression - mark old History nodes as Dormant
    /// 3. Relationship auto-generation - track collaboration span
    ///
    /// Each step is best-effort: failures are logged but do not block
    /// subsequent steps.
    ///
    /// ADR-051 P3: Replaces run_generalization_if_possible(),
    /// self_evaluate_skill_performance(), and auto_generate_relationship()
    /// in loop_memory.rs.
    pub async fn run_post_compaction_tasks(
        &self,
        provider: &dyn MemoryProvider,
        embedding_fn: Option<EmbeddingFn>,
    ) {
        // Step 1: Experience generalization (Path C).
        self.run_generalization_step(provider, embedding_fn).await;

        // Step 2: History compression.
        self.run_history_compression(provider);

        // Step 3: Relationship auto-generation.
        self.run_relationship_generation(provider);
    }

    /// Activate ProceduralNodes that were retrieved and matched the context.
    ///
    /// For each retrieved memory with label "Procedural", increments the
    /// `activation_count`. This tracks how often a procedure is actually
    /// used, feeding into confidence boosting (ADR-057 P1) and future
    /// activation-based retrieval ranking.
    fn activate_procedural_nodes(
        &self,
        provider: &dyn MemoryProvider,
        memories: &[RetrievedMemory],
    ) {
        for memory in memories {
            if memory.label != labels::PROCEDURAL || memory.node_id == 0 {
                continue;
            }

            if let Some(mut node) = provider.get_procedural(memory.node_id).ok().flatten() {
                node.activation_count = node.activation_count.saturating_add(1);
                node.updated_at = chrono::Utc::now();
                if let Err(e) = provider.update_procedural(&node) {
                    tracing::debug!(
                        node_id = memory.node_id,
                        error = %e,
                        "Failed to increment activation_count (non-fatal)"
                    );
                }
            }
        }
    }

    /// Step 1: Experience generalization (Path C).
    async fn run_generalization_step(
        &self,
        provider: &dyn MemoryProvider,
        embedding_fn: Option<EmbeddingFn>,
    ) {
        let config = GeneralizationConfig {
            min_observations: 3,
            max_episodes_scan: 100,
            confidence_boost: 0.05,
            max_confidence: 0.98,
            use_llm: false,
        };

        // Use provided embedding function, or fallback to zero vector.
        let zero_fn: EmbeddingFn = Arc::new(|_| vec![0.0f32; 128]);
        let emb_fn = embedding_fn.unwrap_or(zero_fn);

        match provider.run_generalization(None, &emb_fn, &config).await {
            Ok(result) => {
                if result.nodes_created > 0 || result.nodes_boosted > 0 {
                    tracing::info!(
                        patterns = result.patterns.len(),
                        nodes_created = result.nodes_created,
                        nodes_boosted = result.nodes_boosted,
                        deduplicated = result.patterns_deduplicated,
                        "Path C: generalization completed after compaction"
                    );
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "Generalization failed (non-fatal)");
            }
        }
    }

    /// Step 2: Compress old History autobiographical nodes.
    fn run_history_compression(&self, provider: &dyn MemoryProvider) {
        match provider.compress_history_nodes(10) {
            Ok(compressed) => {
                if compressed > 0 {
                    tracing::info!(
                        compressed,
                        "History compression: marked old History nodes as Dormant"
                    );
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "History compression failed (non-fatal)");
            }
        }
    }

    /// Step 3: Auto-generate Relationship nodes at session-end.
    ///
    /// Checks if the earliest episode is > 30 days old. If so, creates or
    /// updates an AutobiographicalNode with category: Relationship.
    fn run_relationship_generation(&self, provider: &dyn MemoryProvider) {
        use crate::{AutobioCategory, AutobiographicalNode, NodeStatus};

        // Fetch a generous upper bound (not usize::MAX) so the GQL `LIMIT`
        // literal stays within int64 range — the GrafeoDB engine rejects
        // `18446744073709551615` with a syntax error. 10k episodes is far
        // beyond any realistic collaboration span.
        const EPISODES_FOR_RELATIONSHIP: usize = 10_000;

        let episodes = match provider.get_episodes(None, EPISODES_FOR_RELATIONSHIP) {
            Ok(eps) => eps,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to get episodes for relationship tracking");
                return;
            }
        };

        let episode_count = episodes.len() as u32;
        let earliest_time = episodes.iter().map(|e| e.timestamp).min();

        let earliest = match earliest_time {
            Some(t) => t,
            None => return,
        };

        let now = chrono::Utc::now();
        let span_days = (now - earliest).num_days();

        if span_days < 30 {
            return;
        }

        let key = "collaboration_span".to_string();
        let value = format!("已合作 {} 天（{} 次对话记录）", span_days, episode_count);

        match provider.find_autobiographical_by_key(&key) {
            Ok(Some(mut existing)) => {
                existing.value = value;
                existing.updated_at = now;
                if let Err(e) = provider.update_autobiographical(&existing) {
                    tracing::debug!(key = %key, error = %e, "Failed to update Relationship node (non-fatal)");
                } else {
                    tracing::info!(span_days, episode_count, "Updated Relationship node for long-standing collaboration");
                }
            }
            Ok(None) => {
                let node = AutobiographicalNode {
                    id: None,
                    category: AutobioCategory::Relationship,
                    key,
                    value,
                    confidence: 0.9,
                    source_episode_id: None,
                    embedding: None,
                    status: NodeStatus::Active,
                    created_at: now,
                    updated_at: now,
                    source: "user_statement".to_string(),
                    metadata: HashMap::new(),
                };
                if let Err(e) = provider.store_autobiographical(&node) {
                    tracing::debug!(error = %e, "Failed to store Relationship node (non-fatal)");
                } else {
                    tracing::info!(span_days, episode_count, "Created Relationship node for long-standing collaboration");
                }
            }
            Err(e) => {
                tracing::debug!(key = %key, error = %e, "Failed to query Relationship node (non-fatal)");
            }
        }
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
        assert_eq!(config.quality.min_score, 0.0);
        assert!(config.quality.exclude_dormant, "D1 Dormant exclusion on by default");
        assert!(config.enable_graph_expand);
        assert!(config.record_async);
        // auto-injection is OFF by default (per-agent opt-in via manifest
        // `[memory.quality].auto_inject_enabled = true`). Rationale: the LLM
        // already recalls memories via the explicit `memory_recall` tool on
        // the same user-message query, so first-turn auto-inject would
        // duplicate that context.
        assert!(!config.auto_inject_enabled);
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
            abstention_prompt: None,
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
            abstention_prompt: None,
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
            abstention_prompt: None,
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
            abstention_prompt: None,
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
            abstention_prompt: None,
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
            abstention_prompt: None,
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
            abstention_prompt: None,
        };

        let injected = manager.inject(&retrieval);

        // The procedural node should be injected with the behavioral guideline format.
        assert!(
            injected.formatted_text.contains("当") && injected.formatted_text.contains("优先"),
            "Procedural injection should use '当 X 时，优先 Y' format, got: {}",
            injected.formatted_text
        );
    }

    // ── ADR-057 P0: ProceduralNode embedding 必填 ─────────────────────────
    //
    // Verifies that the procedural embedding fallback is deterministic and
    // dimension-compatible with HNSW, and that `procedural_embedding_for`
    // prefers a real `EmbeddingFn` when supplied. These are used by all
    // ProceduralNode creation paths (distill, generalization, manual).

    #[test]
    fn procedural_fallback_embedding_is_deterministic_and_dim_384() {
        let a = procedural_embedding_fallback("hello world");
        let b = procedural_embedding_fallback("hello world");
        let c = procedural_embedding_fallback("another text");
        assert_eq!(a, b, "same text must hash identically");
        assert_ne!(a, c, "different text must hash differently");
        assert_eq!(
            a.len(),
            PROCEDURAL_FALLBACK_DIM,
            "fallback dim must match HnswConfig::default().vector_dim (384)"
        );
    }

    #[test]
    fn procedural_embedding_for_prefers_real_fn_over_fallback() {
        let marker: EmbeddingFn = std::sync::Arc::new(|_text: &str| vec![42.0f32; 8]);
        let v = procedural_embedding_for("any text", Some(&marker));
        assert_eq!(v, vec![42.0f32; 8], "real fn must win over fallback");

        let v = procedural_embedding_for("any text", None);
        assert_eq!(
            v.len(),
            PROCEDURAL_FALLBACK_DIM,
            "fallback used when no fn provided"
        );
    }
}
