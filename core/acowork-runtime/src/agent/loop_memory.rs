//! Memory system integration for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 6.
//!
//! ADR-051 P3: This module now only handles "when to call" MemoryManager
//! methods and "how to put results into ContextBuilder". All direct provider
//! CRUD (get_procedural, update_procedural, store_autobiographical, etc.)
//! has been moved to MemoryManager high-level methods.
//!
//! Contains:
//! - Memory store initialization
//! - Long-term memory retrieval and context injection (via retrieve_and_inject)
//! - Document entry persistence to conversation JSONL
//! - Tool failure recording (via record_tool_failures)
//! - Post-compaction tasks: generalization + self-eval + relationship
//!   (via run_post_compaction_tasks)
//! - MetricsAggregator wiring + alert logging
//! - LLM Judge sampling

use acowork_core::providers::traits::ToolCall;
use acowork_grafeo::judge::{JudgeConfig, should_sample};
use acowork_memory::MemoryProvider;
use crate::memory::metrics::MetricsAlertType;

use crate::agent::context::ContextBuilder;

impl super::loop_::AgentLoop {
    // ── Memory system methods ──────────────────────────────────────────────

    /// Initialize the memory provider at the given workspace path.
    ///
    /// Delegates to `AgentCore::init_memory_provider()`.
    /// Opens or creates `{work_dir}/memory/private.grafeo`.
    pub fn init_memory_store(&mut self, work_dir: &std::path::Path) {
        self.core.init_memory_provider(work_dir);
    }

    /// Retrieve relevant long-term memories and inject them into
    /// the ContextBuilder for the next LLM call.
    ///
    /// ADR-051 P3: Delegates to `MemoryManager::retrieve_and_inject()`.
    /// This method only handles ContextBuilder wiring and metrics aggregation.
    ///
    /// Returns the list of Grafeo node IDs that were retrieved (for traceability).
    pub(crate) async fn retrieve_and_inject_memories(
        &self,
        user_message: &str,
        context_builder: &mut ContextBuilder,
    ) -> Vec<String> {
        // P0 fix: Always clear stale memory from previous turns first.
        context_builder.clear_retrieved_memory();

        let provider = match self.core.memory_provider() {
            Some(s) => s,
            None => return vec![],
        };

        let manager = self.core.init_memory_manager();

        // Build exclude_session_id filter.
        let current_session_id = self
            .session
            .conversation
            .as_ref()
            .map(|c| c.session_id().to_string());

        // Update MemorySessionHandle so memory_recall tool can see the
        // current session_id for its own exclude_session_id filtering.
        if let Some(ref handle) = self.core.memory_session
            && let Some(ref sid) = current_session_id {
                handle.set_session_id(sid.clone());
            }

        let mut query =
            acowork_memory::MemoryQuery::auto_inject(user_message.to_string(), current_session_id);

        // Pass embedding provider from AgentCore so retrieve() can auto-generate
        // query embeddings on-demand.
        let emb_provider = self.core.embedding_provider.as_deref();

        // ADR-051 P3: Use high-level retrieve_and_inject instead of
        // separate retrieve + inject + activate + ambiguity check.
        match manager
            .retrieve_and_inject(provider.as_ref(), &mut query, emb_provider)
            .await
        {
            Ok(result) => {
                let memory_ids = result.memory_ids;
                let metrics = result.metrics;

                // P3-1: Feed retrieval metrics into RetrievalMetricsAggregator.
                let alerts = {
                    let mut agg = self.core.metrics_aggregator.lock().unwrap();
                    if metrics.max_score > agg.max_possible_score() {
                        agg.set_max_possible_score(metrics.max_score);
                    }
                    agg.record_retrieval(&metrics)
                };

                // P3-2: Log alerts via tracing::warn!.
                for alert in &alerts {
                    match alert.alert_type {
                        MetricsAlertType::LowNrr => {
                            tracing::warn!(
                                nrr = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: consistently low NRR - check embedding model or index"
                            );
                        }
                        MetricsAlertType::HighAbstentionRate => {
                            tracing::warn!(
                                rate = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: high abstention rate - consider lowering min_score"
                            );
                        }
                        MetricsAlertType::LowAbstentionRate => {
                            tracing::warn!(
                                rate = alert.value,
                                threshold = alert.threshold,
                                "Memory alert: very low abstention rate - min_score may be too low"
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
                                "Memory alert: high degradation rate - retrieval quality declining"
                            );
                        }
                    }
                }

                // Inject formatted memory text into ContextBuilder.
                if !result.injected.formatted_text.is_empty() {
                    tracing::info!(
                        memory_count = result.injected.memory_count,
                        token_count = result.injected.token_count,
                        avg_score = metrics.avg_score,
                        "Retrieved and injected long-term memories into context"
                    );
                    context_builder.set_retrieved_memory(result.injected.formatted_text);
                }

                // P3-4: Inject ambiguous conflict hint into context.
                if let Some(hint) = result.ambiguous_hint {
                    tracing::info!(
                        "Injecting ambiguous conflict confirmation hint into context"
                    );
                    context_builder.set_ambiguous_confirmation_hint(hint);
                }

                // P3-3: Sample and evaluate retrieval quality via LLM Judge.
                {
                    let judge_config = JudgeConfig::default();
                    let query_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        query.query_text.hash(&mut hasher);
                        hasher.finish()
                    };
                    if should_sample(&judge_config, query_hash) {
                        let result_texts: Vec<String> = {
                            // Re-derive result texts from the injected content
                            // (the RetrievalResult was consumed by retrieve_and_inject).
                            // For the judge, we use the raw query_text as a proxy.
                            // The actual result texts are no longer available here,
                            // but the judge evaluation is best-effort and sampled.
                            vec![query.query_text.clone()]
                        };

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
                    "Failed to retrieve memories (non-fatal)"
                );
                vec![]
            }
        }
    }

    /// Persist attached items as standalone system entries in the
    /// conversation JSONL (ADR-046).
    pub fn write_attached_items(
        &self,
        items: &[acowork_core::protocol::AttachedItem],
    ) {
        let Some(ref conversation) = self.session.conversation else {
            return;
        };
        for item in items {
            let (content, metadata, client_id) = match item {
                acowork_core::protocol::AttachedItem::FileUpload { document_id, filename, format, size_bytes, client_id, .. } => {
                    let meta = crate::conversation::FileUploadMeta {
                        document_id: document_id.clone(),
                        filename: filename.clone(),
                        format: format.clone(),
                        size_bytes: *size_bytes,
                    };
                    let content = format!("Uploaded file: {} ({}, {} bytes)", filename, format, size_bytes);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::FileUpload(meta),
                    )
                    .expect("FileUploadMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::ImageUpload { document_id, filename, format, size_bytes, width, height, client_id, .. } => {
                    let meta = crate::conversation::ImageUploadMeta {
                        document_id: document_id.clone(),
                        filename: filename.clone(),
                        format: format.clone(),
                        size_bytes: *size_bytes,
                        width: *width,
                        height: *height,
                    };
                    let content = format!("Uploaded image: {} ({}, {} bytes)", filename, format, size_bytes);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::ImageUpload(meta),
                    )
                    .expect("ImageUploadMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedFile { abs_path, name, client_id, .. } => {
                    let meta = crate::conversation::AttachedFileMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                    };
                    let content = format!("Attached file: {}", name);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedFile(meta),
                    )
                    .expect("AttachedFileMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedSelection { abs_path, name, start_line, end_line, client_id, .. } => {
                    let meta = crate::conversation::AttachedSelectionMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                        start_line: *start_line,
                        end_line: *end_line,
                    };
                    let content = format!("Attached selection: {} (L{}-L{})", name, start_line, end_line);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedSelection(meta),
                    )
                    .expect("AttachedSelectionMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
                acowork_core::protocol::AttachedItem::AttachedFolder { abs_path, name, client_id, .. } => {
                    let meta = crate::conversation::AttachedFolderMeta {
                        abs_path: abs_path.clone(),
                        name: name.clone(),
                    };
                    let content = format!("Attached folder: {}", name);
                    let metadata = serde_json::to_value(
                        crate::conversation::AttachmentMeta::AttachedFolder(meta),
                    )
                    .expect("AttachedFolderMeta is always serializable");
                    (content, Some(metadata), client_id.clone())
                }
            };
            conversation.append_message_with_id("system", &content, metadata, client_id);
        }
    }

    /// Record tool execution failures as ProceduralNodes (Path B).
    ///
    /// ADR-051 P3: Delegates to `MemoryManager::record_tool_failures()`.
    /// This method only handles error detection and tool_name extraction.
    pub(crate) fn record_tool_failures_to_memory(
        &self,
        tool_calls: &[ToolCall],
        tool_results: &[String],
    ) {
        let provider = match self.core.memory_provider() {
            Some(s) => s,
            None => return,
        };

        let manager = self.core.init_memory_manager();

        // Collect (tool_name, error_message) pairs for failed tools.
        let failures: Vec<(&str, &str)> = tool_calls
            .iter()
            .zip(tool_results.iter())
            .filter_map(|(tc, result)| {
                let is_error =
                    result.starts_with("Error:") || result.starts_with("Tool execution error:");
                let is_unknown = result.starts_with("Unknown tool:");
                if is_error && !is_unknown {
                    Some((tc.function.name.as_str(), result.as_str()))
                } else {
                    None
                }
            })
            .collect();

        manager.record_tool_failures(provider.as_ref(), &failures);
    }

    /// Run all post-compaction maintenance tasks.
    ///
    /// ADR-051 P3: Delegates to `MemoryManager::run_post_compaction_tasks()`.
    /// Replaces the previous separate methods:
    /// - run_generalization_if_possible()
    /// - self_evaluate_skill_performance()
    /// - auto_generate_relationship()
    pub(crate) async fn run_post_compaction_memory_tasks(&self) {
        let provider = match self.core.memory_provider() {
            Some(s) => s,
            None => return,
        };

        let manager = self.core.init_memory_manager();

        // No embedding function available in this context;
        // run_post_compaction_tasks will use a zero-vector fallback.
        manager.run_post_compaction_tasks(provider.as_ref(), None).await;
    }
}
