//! Memory system integration for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 6.
//!
//! Contains:
//! - Memory store initialization
//! - Long-term memory retrieval and context injection
//! - Document entry persistence to conversation JSONL
//! - Tool failure → ProceduralNode recording (Path B)
//! - Self-evaluation → Autobiographical Limitation nodes (P2-1)
//! - Relationship auto-generation at session-end (P2-2)
//! - MetricsAggregator wiring + alert logging (P3-1, P3-2)
//! - Ambiguous conflict confirmation hint injection (P3-4)

use std::collections::HashMap;

use acowork_core::providers::traits::ToolCall;
use acowork_grafeo::judge::{JudgeConfig, should_sample};
use acowork_grafeo::retrieval_metrics::{MetricsAlertType, OnlineRetrievalMetrics};

use crate::agent::context::ContextBuilder;

impl super::loop_::AgentLoop {
    // ── Memory system methods ──────────────────────────────────────────────

    /// Initialize the Grafeo memory store at the given workspace path.
    ///
    /// Delegates to `AgentCore::init_memory_store()`.
    /// Opens or creates `{work_dir}/memory/private.grafeo`.
    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        self.core.init_memory_store(work_dir);
    }

    /// Retrieve relevant long-term memories from Grafeo and inject them into
    /// the ContextBuilder for the next LLM call.
    ///
    /// Runs once per `run()` invocation, before the first LLM iteration.
    /// When the memory store is unavailable, this is a silent no-op.
    ///
    /// Returns the list of Grafeo node IDs that were retrieved (P2-4 fix).
    /// These IDs are passed to `record_turn_to_memory` so that future
    /// retrieval can trace which memories influenced each turn.
    pub(crate) async fn retrieve_and_inject_memories(
        &self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
    ) -> Vec<String> {
        // P0 fix: Always clear stale memory from previous turns first.
        // ContextBuilder is reused across turns (SessionTask loop), so
        // without this, stale memory leaks into the next LLM call.
        context_builder.clear_retrieved_memory();

        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return vec![], // No store available, already cleared above
        };

        let manager = self.core.init_memory_manager();

        // Build exclude_session_id filter to avoid re-injecting Episode
        // summaries that are already in the current session's context window.
        let current_session_id = self
            .session
            .conversation
            .as_ref()
            .map(|c| c.session_id().to_string());

        // Update MemorySessionHandle so memory_recall tool can see the
        // current session_id for its own exclude_session_id filtering.
        if let Some(ref handle) = self.core.memory_session {
            if let Some(ref sid) = current_session_id {
                handle.set_session_id(sid.clone());
            }
        }

        let mut query =
            acowork_memory::MemoryQuery::auto_inject(user_message.to_string(), current_session_id);

        // Pass embedding provider from AgentCore so retrieve() can auto-generate
        // query embeddings on-demand (Ollama local → Remote fallback chain).
        let emb_provider = self.core.embedding_provider.as_deref();

        // P2-4 fix: Use retrieve + inject separately (instead of process_turn)
        // so we can capture the node IDs of retrieved memories for traceability.
        match manager.retrieve(store, &mut query, emb_provider).await {
            Ok(retrieval) => {
                // Capture node IDs before inject (inject discards the RetrievalResult)
                let memory_ids: Vec<String> = retrieval
                    .memories
                    .iter()
                    .filter(|m| m.node_id != 0) // 0 = RAG result, not Grafeo local
                    .map(|m| m.node_id.to_string())
                    .collect();

                let metrics = retrieval.metrics.clone();

                // P3-1: Feed retrieval metrics into MetricsAggregator.
                // Convert acowork-memory::RetrievalMetrics →
                // acowork-grafeo::OnlineRetrievalMetrics.
                let online_metrics = OnlineRetrievalMetrics {
                    result_count: metrics.result_count,
                    avg_score: metrics.avg_score,
                    max_score: metrics.max_score,
                    abstention_triggered: metrics.abstention_triggered,
                    retrieval_level: metrics.retrieval_level,
                    graph_expand_nodes: metrics.graph_expand_nodes,
                    hint_type: match metrics.hint_type {
                        acowork_memory::HintType::Semantic => {
                            acowork_grafeo::retrieval_metrics::HintType::Semantic
                        }
                        acowork_memory::HintType::Factual => {
                            acowork_grafeo::retrieval_metrics::HintType::FullText
                        }
                        acowork_memory::HintType::Relational => {
                            acowork_grafeo::retrieval_metrics::HintType::Hybrid
                        }
                        acowork_memory::HintType::Identity => {
                            acowork_grafeo::retrieval_metrics::HintType::GraphExpand
                        }
                    },
                };

                let alerts = {
                    let mut agg = self.core.metrics_aggregator.lock().unwrap();
                    // Update max_possible_score if we have a better reference.
                    // RRF hybrid scores are typically 0.01–0.05, so use a
                    // sensible default of 1.0 unless we observe higher.
                    if online_metrics.max_score > agg.max_possible_score() {
                        agg.set_max_possible_score(online_metrics.max_score);
                    }
                    agg.record_retrieval(&online_metrics)
                };

                // P3-2: Log alerts via tracing::warn! so Desktop App can
                // subscribe via the log stream.
                for alert in &alerts {
                    match alert.alert_type {
                        MetricsAlertType::LowNrr => {
                            tracing::warn!(
                                nrr = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: consistently low NRR — check embedding model or index"
                            );
                        }
                        MetricsAlertType::HighAbstentionRate => {
                            tracing::warn!(
                                rate = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: high abstention rate — consider lowering min_score"
                            );
                        }
                        MetricsAlertType::LowAbstentionRate => {
                            tracing::warn!(
                                rate = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: very low abstention rate — min_score may be too low"
                            );
                        }
                        MetricsAlertType::LowConflictAccuracy => {
                            tracing::warn!(
                                accuracy = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: conflict resolution accuracy below threshold"
                            );
                        }
                        MetricsAlertType::HighDegradationRate => {
                            tracing::warn!(
                                rate = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: high degradation rate — retrieval quality declining"
                            );
                        }
                    }
                }

                // Activate ProceduralNodes: increment activation_count for
                // retrieved procedures whose trigger matches the query context.
                self.activate_procedural_nodes(store, &retrieval.memories);

                let injected = manager.inject(&retrieval);
                if !injected.formatted_text.is_empty() {
                    tracing::info!(
                        memory_count = injected.memory_count,
                        token_count = injected.token_count,
                        avg_score = metrics.avg_score,
                        "Retrieved and injected long-term memories into context"
                    );
                    context_builder.set_retrieved_memory(injected.formatted_text);
                }

                // P3-4: Check for ambiguous memory conflicts that need
                // user confirmation. If ≥ 3 pending conflicts, inject a
                // hint into the next turn's context to guide the Agent to
                // naturally ask the user for disambiguation.
                if let Ok(true) = store.should_trigger_confirmation() {
                    if let Ok(Some(hint)) = store.generate_confirmation_hint() {
                        tracing::info!(
                            "Injecting ambiguous conflict confirmation hint into context"
                        );
                        context_builder.set_ambiguous_confirmation_hint(hint);
                    }
                }

                // P3-3: Sample and evaluate retrieval quality via LLM Judge.
                // Uses deterministic sampling (10% of retrievals) and evaluates
                // only the top-3 results using the cheapest model.
                {
                    let judge_config = JudgeConfig::default();
                    let query_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        query.query_text.hash(&mut hasher);
                        hasher.finish()
                    };
                    if should_sample(&judge_config, query_hash) {
                        let result_texts: Vec<String> = retrieval
                            .memories
                            .iter()
                            .take(judge_config.top_k)
                            .map(|m| m.content.clone())
                            .collect();

                        // Spawn a background evaluation — don't block the
                        // retrieval pipeline. Result is logged and fed back
                        // into the MetricsAggregator for trend tracking.
                        let provider = self.core.provider.clone();
                        let model = judge_config.model.clone();
                        let query_text = query.query_text.clone();
                        let metrics_agg = self.core.metrics_aggregator.clone();
                        tokio::spawn(async move {
                            let result = crate::memory::evaluate_retrieval_llm(
                                provider.as_ref(),
                                &JudgeConfig {
                                    model,
                                    ..judge_config
                                },
                                &query_text,
                                &result_texts,
                            )
                            .await;
                            tracing::info!(
                                score = result.relevance_score,
                                reason = %result.reason,
                                "P3-3: LLM Judge evaluated retrieval quality"
                            );
                            // Feed the Judge score back into the MetricsAggregator.
                            if let Ok(mut agg) = metrics_agg.lock() {
                                agg.record_judge_score(result.relevance_score);
                            }
                        });
                    }
                }

                memory_ids
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to retrieve memories from Grafeo (non-fatal)"
                );
                vec![]
            }
        }
    }

    /// Write document upload entries to the conversation JSONL.
    ///
    /// Each document is persisted as a `ConversationEntry` with `role: "system"`
    /// and `metadata.type: "document_upload"` so that the Desktop App can render
    /// document chips when loading historical sessions.
    pub fn write_document_entries(&self, documents: &[serde_json::Value]) {
        if let Some(ref conversation) = self.session.conversation {
            for doc in documents {
                let filename = doc.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                let format = doc.get("format").and_then(|v| v.as_str()).unwrap_or("");
                let size = doc.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let content = format!("Uploaded file: {} ({}, {} bytes)", filename, format, size);
                let metadata = serde_json::json!({
                    "type": "document_upload",
                    "document_id": doc.get("id"),
                    "filename": filename,
                    "format": format,
                    "size_bytes": size,
                    "path": doc.get("abs_path"),
                });
                conversation.append_message("system", &content, Some(metadata));
            }
        }
    }

    /// Record tool execution failures as ProceduralNodes (Path B).
    ///
    /// Scans the tool results for errors and creates low-confidence
    /// ProceduralNodes via `MemoryManager::record_procedural_from_failure()`.
    /// This is a best-effort operation — failures are logged but never
    /// block the main agent loop.
    ///
    /// Only records failures for known tools (not "Unknown tool" errors,
    /// which indicate a registry issue, not a skill failure).
    pub(crate) fn record_tool_failures_to_memory(
        &self,
        tool_calls: &[ToolCall],
        tool_results: &[String],
    ) {
        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return,
        };

        let manager = self.core.init_memory_manager();

        for (tc, result) in tool_calls.iter().zip(tool_results.iter()) {
            // Detect tool failure from the result string.
            // Failure patterns: "Error:", "Tool execution error:"
            // Skip "Unknown tool:" errors (registry issue, not skill failure).
            let is_error =
                result.starts_with("Error:") || result.starts_with("Tool execution error:");
            let is_unknown = result.starts_with("Unknown tool:");

            if is_error && !is_unknown {
                if let Err(e) =
                    manager.record_procedural_from_failure(store, &tc.function.name, result)
                {
                    tracing::debug!(
                        tool_name = %tc.function.name,
                        error = %e,
                        "Failed to record ProceduralNode from tool failure (non-fatal)"
                    );
                }
            }
        }
    }

    /// Activate ProceduralNodes that were retrieved and matched the context.
    ///
    /// For each retrieved memory with label "Procedural", increments the
    /// `activation_count` in the Grafeo store. This tracks how often a
    /// procedure is actually used, which feeds into self-evaluation (P2-1)
    /// and confidence boosting.
    fn activate_procedural_nodes(
        &self,
        store: &acowork_grafeo::grafeo::GrafeoStore,
        memories: &[crate::memory::manager::RetrievedMemory],
    ) {
        use acowork_grafeo::types::labels;

        for memory in memories {
            if memory.label != labels::PROCEDURAL || memory.node_id == 0 {
                continue;
            }

            let node_id = grafeo_common::types::NodeId::new(memory.node_id);
            if let Some(mut node) = store.get_procedural(node_id).ok().flatten() {
                node.activation_count = node.activation_count.saturating_add(1);
                node.updated_at = chrono::Utc::now();
                if let Err(e) = store.update_procedural(&node) {
                    tracing::debug!(
                        node_id = memory.node_id,
                        error = %e,
                        "Failed to increment activation_count (non-fatal)"
                    );
                }
            }
        }
    }

    /// Run experience generalization after a successful compaction (Path C).
    ///
    /// Triggers rule-based pattern detection from unconsolidated episodes.
    /// If enough episodes exist (> min_observations, default 3), patterns
    /// are extracted and stored as ProceduralNodes with
    /// `learned_from = "generalization"`.
    ///
    /// This is a best-effort operation — failures are logged but never
    /// block the main agent loop. LLM-driven pattern discovery is
    /// deferred until a `TripleExtractorLlm` adapter is implemented.
    pub(crate) async fn run_generalization_if_possible(&self) {
        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return,
        };

        use acowork_grafeo::consolidation::generalization::GeneralizationConfig;

        let config = GeneralizationConfig {
            min_observations: 3,
            max_episodes_scan: 100,
            confidence_boost: 0.05,
            max_confidence: 0.98,
            use_llm: false, // No LLM adapter yet; rule-based only
        };

        // Dummy embedding function — returns a zero vector.
        // Full embeddings will be filled during the next consolidation cycle.
        // Using a function pointer (fn) instead of a closure to satisfy
        // Send + Sync requirements for the async generalization call.
        fn zero_embedding(_text: &str) -> Vec<f32> {
            vec![0.0f32; acowork_grafeo::types::DEFAULT_EMBEDDING_DIM]
        }
        let zero_embedding_arc: std::sync::Arc<dyn Fn(&str) -> Vec<f32> + Send + Sync> =
            std::sync::Arc::new(zero_embedding);

        match store
            .run_generalization(None, &zero_embedding_arc, &config)
            .await
        {
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
                tracing::debug!(
                    error = %e,
                    "Generalization failed (non-fatal)"
                );
            }
        }

        // P2-3: Compress History autobiographical nodes if > 10.
        match store.compress_history_nodes(10) {
            Ok(compressed) => {
                if compressed > 0 {
                    tracing::info!(
                        compressed,
                        "History compression: marked old History nodes as Dormant"
                    );
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "History compression failed (non-fatal)"
                );
            }
        }
    }

    /// Self-evaluate skill performance and create Limitation nodes (P2-1).
    ///
    /// Scans all ProceduralNodes, groups them by `source_skill`, and
    /// calculates the success rate for each skill. If a skill's success
    /// rate falls below 60% with at least 5 total observations
    /// (success + fail), an `AutobiographicalNode` with
    /// `category: Limitation` is created or updated.
    ///
    /// Called after generalization (Path C) during the compaction flow.
    /// This is a best-effort operation — failures are logged but never
    /// block the main agent loop.
    pub(crate) fn self_evaluate_skill_performance(&self) {
        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return,
        };

        use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, NodeStatus};

        // Gather all procedural nodes and compute per-skill success rates.
        let nodes = match store.get_all_procedural_nodes() {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to get procedural nodes for self-evaluation");
                return;
            }
        };

        // Group by source_skill, accumulating success/fail counts.
        let mut skill_stats: HashMap<String, (u32, u32)> = HashMap::new();
        for node in &nodes {
            if let Some(ref skill) = node.source_skill {
                let entry = skill_stats.entry(skill.clone()).or_insert((0, 0));
                entry.0 += node.success_count;
                entry.1 += node.fail_count;
            }
        }

        let min_observations: u32 = 5;
        let max_success_rate: f32 = 0.60; // below this → Limitation

        for (skill, (success, fail)) in skill_stats {
            let total = success + fail;
            if total < min_observations {
                continue;
            }

            let success_rate = success as f32 / total as f32;
            if success_rate >= max_success_rate {
                continue;
            }

            // Create or update a Limitation node for this skill.
            let key = format!("skill_{}", skill.to_lowercase());
            let value = format!(
                "{} 成功率仅 {:.0}%（{} 次成功 / {} 次失败）",
                skill,
                success_rate * 100.0,
                success,
                fail
            );

            // Check if a Limitation node already exists for this skill.
            match store.find_autobiographical_by_key(&key) {
                Ok(Some(mut existing)) => {
                    // Update the existing node with new stats.
                    existing.value = value;
                    existing.updated_at = chrono::Utc::now();
                    if let Err(e) = store.update_autobiographical(&existing) {
                        tracing::debug!(
                            key = %key,
                            error = %e,
                            "Failed to update Limitation node (non-fatal)"
                        );
                    } else {
                        tracing::info!(
                            skill = %skill,
                            success_rate = success_rate,
                            "Updated existing Limitation node for skill"
                        );
                    }
                }
                Ok(None) => {
                    // Create a new Limitation node.
                    let node = AutobiographicalNode {
                        id: None,
                        category: AutobioCategory::Limitation,
                        key,
                        value,
                        confidence: 0.8,
                        source_episode_id: None,
                        embedding: None,
                        status: NodeStatus::Active,
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                        metadata: HashMap::new(),
                    };
                    if let Err(e) = store.store_autobiographical(&node) {
                        tracing::debug!(
                            skill = %skill,
                            error = %e,
                            "Failed to store Limitation node (non-fatal)"
                        );
                    } else {
                        tracing::info!(
                            skill = %skill,
                            success_rate = success_rate,
                            observations = total,
                            "Created Limitation node for low-performing skill"
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        key = %key,
                        error = %e,
                        "Failed to query Limitation node (non-fatal)"
                    );
                }
            }
        }
    }

    /// Auto-generate Relationship nodes at session-end (P2-2).
    ///
    /// Per ADR-P2-004: checks if the earliest episode in Grafeo is more
    /// than 30 days old, indicating a long-standing collaboration. If so,
    /// creates or updates an `AutobiographicalNode { category: Relationship }`.
    ///
    /// Since AgentCowork doesn't have explicit user identity, we use a
    /// generic key "collaboration_span" to track the overall partnership
    /// duration. In the future, when user identity is available, this
    /// can be extended to per-user relationship tracking.
    ///
    /// This is a best-effort operation — failures are logged but never
    /// block the session close flow.
    pub(crate) fn auto_generate_relationship(&self) {
        let store = match self.core.memory_store() {
            Some(s) => s,
            None => return,
        };

        use acowork_grafeo::types::{AutobioCategory, AutobiographicalNode, NodeStatus, labels};
        use grafeo_common::types::Value;

        // Find the earliest episode in the Grafeo store.
        let db = store.db();
        let graph = db.graph_store();
        let episodic_ids = graph.nodes_by_label(labels::EPISODIC);

        let mut earliest_time: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut episode_count: u32 = 0;

        for id in episodic_ids {
            if let Some(n) = db.get_node(id) {
                episode_count += 1;
                if let Some(ts) = n.get_property("created_at").and_then(Value::as_timestamp) {
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

        let earliest = match earliest_time {
            Some(t) => t,
            None => return, // No episodes → nothing to track
        };

        let now = chrono::Utc::now();
        let span_days = (now - earliest).num_days();

        // Only create/update Relationship if collaboration spans > 30 days.
        let min_days: i64 = 30;
        if span_days < min_days {
            return;
        }

        let key = "collaboration_span".to_string();
        let value = format!("已合作 {} 天（{} 次对话记录）", span_days, episode_count);

        // Check if a Relationship node already exists.
        match store.find_autobiographical_by_key(&key) {
            Ok(Some(mut existing)) => {
                existing.value = value;
                existing.updated_at = now;
                if let Err(e) = store.update_autobiographical(&existing) {
                    tracing::debug!(
                        key = %key,
                        error = %e,
                        "Failed to update Relationship node (non-fatal)"
                    );
                } else {
                    tracing::info!(
                        span_days,
                        episode_count,
                        "Updated Relationship node for long-standing collaboration"
                    );
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
                    metadata: HashMap::new(),
                };
                if let Err(e) = store.store_autobiographical(&node) {
                    tracing::debug!(
                        error = %e,
                        "Failed to store Relationship node (non-fatal)"
                    );
                } else {
                    tracing::info!(
                        span_days,
                        episode_count,
                        "Created Relationship node for long-standing collaboration"
                    );
                }
            }
            Err(e) => {
                tracing::debug!(
                    key = %key,
                    error = %e,
                    "Failed to query Relationship node (non-fatal)"
                );
            }
        }
    }
}
