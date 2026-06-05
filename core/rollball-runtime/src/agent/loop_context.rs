//! Context management for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 1).
//! Contains all methods related to context window management:
//! - Token budget calculation
//! - History trimming (FIFO + emergency)
//! - LLM-based context compaction
//! - Model resolution for compaction/distillation
//! - Runtime config application (affects context limits)

use std::sync::Arc;

use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::providers::traits::Provider;

use crate::agent::loop_::{AgentLoop, ChunkEvent, SessionChunkEvent};

impl AgentLoop {
    /// Update the LLM provider at runtime (e.g., after receiving a hot-pushed
    /// LLMConfigDelivery from Gateway).
    /// `provider_id` is the Vault provider ID (not protocol name) for
    /// compact_model lookup.
    pub fn update_provider(
        &mut self,
        new_provider: Arc<dyn Provider>,
        model: String,
        provider_id: Option<String>,
    ) {
        self.core.update_provider(new_provider, model);
        if let Some(pid) = provider_id {
            self.core.current_provider_id = Some(pid);
        }
    }

    /// Update gateway model capabilities at runtime (e.g., after receiving a
    /// hot-pushed LLMConfigDelivery from Gateway).
    /// The capabilities are stored keyed by model ID for multi-model support.
    pub fn update_gateway_model_capabilities(&mut self, model_id: &str, caps: ModelCapabilitiesInfo) {
        self.core.update_gateway_model_capabilities(model_id, caps);
    }

    /// Update the max output tokens limit from Gateway config.
    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        self.core.update_max_output_tokens_limit(limit);
    }

    /// Apply runtime config overrides from Gateway.
    pub fn apply_runtime_config(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        shell_approval_threshold: Option<String>,
    ) {
        self.core.apply_runtime_config(max_output_tokens, max_iterations, temperature, system_prompt_override, shell_approval_threshold);
    }

    /// Get the context window budget for history trimming.
    /// Uses Gateway model capabilities (context_window) if available,
    /// otherwise falls back to config.history_max_tokens.
    pub(crate) fn context_trim_budget(&self, model_name: &str) -> u64 {
        self.core.context_trim_budget(model_name)
    }

    /// Trim history to fit within the context window budget.
    ///
    /// The budget comes from [`context_trim_budget`] →
    /// [`ModelCapabilitiesInfo::effective_input_budget`], which already
    /// reserves output space (capped at `max_output_tokens_limit`, default 32K).
    /// No additional margin is applied here — [`compact_history_if_needed`]
    /// provides early warning at 80% usage.
    pub(crate) fn trim_history_to_budget(&mut self, model_name: &str) {
        let budget = self.context_trim_budget(model_name);

        // Sync HistoryManager::max_tokens to the actual model budget so
        // trim_fifo uses the correct threshold. Without this, max_tokens
        // remains at the static config default (128K) even after model
        // switch, capabilities update, or max_output_tokens change.
        self.session.history.set_max_tokens(budget);

        // Stage 1: FIFO trim oldest non-system messages until within budget
        self.session.history.trim_fifo();

        // Stage 2: If still over budget after FIFO, use emergency trim as safety net
        if self.session.history.token_count() > budget {
            self.session.history.emergency_trim();
        }

        // Also truncate any single message that exceeds per-message limit
        self.session.history.truncate_large_messages(budget / 4);
    }

    /// Resolve the model to use for session distillation or compaction.
    ///
    /// Uses [`crate::token::count_text`] — the single unified token counting API.
    ///
    /// Priority order:
    /// 1. Provider's configured `compact_model` from provider_list (read from disk)
    /// 2. Current model (fallback when compact model unavailable or context too small)
    pub(crate) fn resolve_distill_model(&self, content_text: &str) -> String {
        let current_model = self.resolve_current_model(None);
        let estimated_tokens = crate::token::count_text(content_text, &current_model) as u64;

        // Path 1: resolve compact_model from current provider (in-memory)
        let compact_model = self
            .core
            .current_provider_id
            .as_ref()
            .and_then(|pid| self.core.provider_compact_models.get(pid))
            .and_then(|cm| cm.clone());
        if let Some(ref compact_model) = compact_model {
            if let Some(cap) = self
                .core
                .gateway_model_capabilities
                .get(compact_model)
            {
                if cap.context_window >= estimated_tokens {
                    tracing::info!(
                        compact_model = %compact_model,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Using provider's compact model for distillation"
                    );
                    return compact_model.clone();
                }
                tracing::warn!(
                    compact_model = %compact_model,
                    context_window = cap.context_window,
                    estimated_tokens,
                    "Provider compact model context_window too small, falling back"
                );
            } else {
                tracing::warn!(
                    compact_model = %compact_model,
                    "Provider compact model not found in capabilities, falling back"
                );
            }
        }

        // Path 2: compact model unavailable or context too small —
        // fall back to the session's current model.
        let current_model = self.resolve_current_model(None);
        tracing::info!(
            current_model = %current_model,
            estimated_tokens,
            "Compact model not available or insufficient, using current model for distillation"
        );
        current_model
    }

    /// Check context usage after LLM response and trigger compaction if needed.
    ///
    /// Per [ADR-011], this implements the three-stage compaction strategy:
    /// - 80% usage → LLM-based compaction (`compact_via_llm` + `replace_middle_with_summary`)
    /// - 95% usage → emergency trim (safety net)
    ///
    /// When `force` is true (manual trigger from user), the 80% threshold is
    /// bypassed and compaction proceeds regardless of current usage percentage.
    ///
    /// Called after each LLM response (force=false) and on manual user trigger
    /// (force=true via `SessionMessage::CompactContext`).
    pub(crate) async fn compact_history_if_needed(
        &mut self,
        model_name: &str,
        force: bool,
    ) {
        /// Number of conversational rounds to keep at the tail after compaction.
        /// Each round starts with a User message, so this keeps the last N user
        /// messages and everything after them.
        const KEEP_LAST_ROUNDS: usize = 3;
        let budget = self.context_trim_budget(model_name);
        let current_tokens = self.session.history.token_count();

        if budget == 0 {
            return;
        }

        let usage_percent = (current_tokens as f64 / budget as f64) * 100.0;

        // Stage 2: 80% → LLM-based compaction (or force=true bypasses threshold)
        if force || usage_percent >= 80.0 {
            tracing::info!(
                usage_percent = ?usage_percent,
                current_tokens,
                budget,
                force,
                "Triggering LLM compaction"
            );

            // Notify frontend that compaction has started (both manual and auto paths).
            if let Some(ref tx) = self.core.on_chunk {
                let _ = tx.send(SessionChunkEvent {
                    session_id: self.core.session_id.clone().unwrap_or_default(),
                    event: ChunkEvent::CompactingStarted,
                }).await;
            }

            // Build combined text from history for model-aware token counting.
            let combined_text: String = self.session.history.messages()
                .iter()
                .fold(String::new(), |mut acc, m| {
                    acc.push_str(&m.content);
                    acc.push('\n');
                    acc
                });
            let compact_model = self.resolve_distill_model(&combined_text);
            let system_prompt = self
                .core
                .system_prompt_override
                .as_deref()
                .unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT);
            let provider = self.core.provider.clone();
            let memory_store = self.core.memory_store().cloned();

            match self
                .session
                .history
                .compact_via_llm(provider.as_ref(), &compact_model, system_prompt)
                .await
            {
                Ok(summary) => {
                    let stripped = crate::episode_distill::strip_metadata_blocks(&summary);
                    let removed = self.session.history.replace_middle_with_summary(&stripped, KEEP_LAST_ROUNDS);

                    // Write compaction summary to Grafeo
                    let session_id = self
                        .session
                        .conversation
                        .as_ref()
                        .map(|c| c.session_id().to_string())
                        .unwrap_or_default();
                    crate::episode_distill::EpisodeDistiller::write_summary_to_grafeo(
                        &summary,
                        &session_id,
                        &memory_store,
                        self.core.embedding_provider.as_deref(),
                    ).await;

                    // Mark session as compacted (zero new messages since compaction)
                    self.session.is_compacted = true;

                    // Recompute usage after compaction for stage 3 check
                    let new_tokens = self.session.history.token_count();
                    let new_usage = if budget > 0 {
                        (new_tokens as f64 / budget as f64) * 100.0
                    } else {
                        0.0
                    };

                    tracing::info!(
                        removed,
                        summary_len = summary.len(),
                        before_tokens = current_tokens,
                        after_tokens = new_tokens,
                        before_usage = ?usage_percent,
                        after_usage = ?new_usage,
                        "LLM compaction completed"
                    );

                    // Stage 3: 95% → emergency trim (safety net, even after compaction)
                    if new_usage >= 95.0 {
                        let em_removed = self.session.history.emergency_trim();
                        tracing::warn!(
                            em_removed,
                            after_usage = ?new_usage,
                            "Emergency trim performed after compaction (still >= 95%)"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "LLM compaction failed, falling back to FIFO + emergency trim"
                    );
                    self.session.history.trim_fifo();
                    if self.session.history.token_count() > budget {
                        self.session.history.emergency_trim();
                    }
                }
            }

            // Notify frontend that compaction has finished, so it can clear
            // the "compacting..." indicator (both success and error paths).
            // Also send updated context usage so the frontend shows the new
            // token count and percentage after compaction.
            if let Some(ref tx) = self.core.on_chunk {
                let session_id = self.core.session_id.clone().unwrap_or_default();
                let _ = tx.try_send(SessionChunkEvent {
                    session_id: session_id.clone(),
                    event: ChunkEvent::CompactingEnded,
                });

                // Compute and send updated context usage after compaction.
                // The history token count has changed, but the frontend still
                // shows the old number from the last LLM API response.
                let caps = self.core.gateway_model_capabilities.get(model_name);
                if let Some(caps) = caps {
                    let usable = caps.effective_input_budget(self.core.max_output_tokens_limit);
                    let total_tokens = self.session.history.token_count();
                    let usage_percent = if usable > 0 {
                        ((total_tokens as f64 / usable as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };
                    let ctx_info = rollball_core::protocol::ContextUsageInfo {
                        context_window: caps.context_window,
                        input_tokens: total_tokens,
                        output_tokens: 0,
                        total_tokens,
                        max_input_tokens: caps.max_input_tokens,
                        usable_context: usable,
                        usage_percent,
                    };
                    let _ = tx.try_send(SessionChunkEvent {
                        session_id,
                        event: ChunkEvent::ContextUsage(ctx_info),
                    });
                }
            }
        } else if usage_percent >= 95.0 {
            // Stage 3: emergency trim without attempting compaction
            // (when usage jumps directly to >= 95%)
            let removed = self.session.history.emergency_trim();
            tracing::warn!(
                removed,
                usage_percent = ?usage_percent,
                current_tokens,
                budget,
                "Emergency trim performed (usage >= 95%)"
            );
        }
    }

    // ── Sub-methods extracted from execute_single_iteration (ADR-014 Phase 1) ──

    /// ① Budget pre-check — validate that the estimated token count is
    /// within budget before proceeding with the LLM call.
    ///
    /// Returns `Err(BudgetExceeded)` if budget is denied, otherwise `Ok(())`.
    /// Warnings are appended to history as system messages.
    pub(crate) fn check_budget_and_warn(&mut self) -> crate::error::Result<()> {
        use crate::agent::budget_guard::BudgetCheckResult;
        use crate::agent::session_state::SessionStatus;
        use rollball_core::providers::traits::{ChatMessage, MessageRole};

        let estimated_tokens = self.session.history.estimate_total_tokens() + 500; // +500 for new response
        match self.session.budget_guard.check(estimated_tokens) {
            BudgetCheckResult::Allowed => {}
            BudgetCheckResult::Exceeded { reason, action } => {
                tracing::warn!(reason = %reason, action = %action, "Budget exceeded");
                match action.as_str() {
                    "deny" => {
                        self.transition_status(SessionStatus::Idle);
                        return Err(crate::error::RuntimeError::BudgetExceeded(reason));
                    }
                    "warn" => {
                        self.session.history.append(ChatMessage {
                            role: MessageRole::User,
                            content: format!("[System Warning] {reason}"),
                            name: Some("system".to_string()),
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// ②.5 Build the chat request from context + history, including MCP tool merge.
    ///
    /// This consolidates: todo injection, context build, logging, and MCP tool
    /// merge into a single method so that `execute_single_iteration` reads as
    /// a high-level orchestration.
    pub(crate) fn build_chat_request(
        &mut self,
        context_builder: &mut crate::agent::context::ContextBuilder,
        current_model: &str,
    ) -> rollball_core::providers::traits::ChatRequest {
        // Inject current todo list into system prompt before building
        context_builder.set_todo_context(self.session.format_todos());
        let mut chat_request = context_builder.build(
            &self.core.manifest,
            &self.session.history,
            self.get_model_capabilities(current_model),
            self.core.max_output_tokens_limit,
        );

        tracing::info!(
            request_messages_count = chat_request.messages.len(),
            request_model = %chat_request.model,
            request_max_tokens = ?chat_request.max_tokens,
            request_tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            history_tokens = self.session.history.token_count(),
            "Built chat request for LLM (after preemptive trim)"
        );

        // Merge MCP tool definitions into the LLM request right before
        // injection. MCP tools are kept separate from active_tools and
        // only mixed here (LLM injection) + in debug snapshot capture.
        if let Some(ref mut tools) = chat_request.tools {
            for tool in &self.core.all_tools {
                let spec = tool.spec();
                if spec.name.starts_with("mcp:") {
                    let val = serde_json::to_value(&spec).unwrap_or_default();
                    tools.push(val);
                }
            }
        }

        chat_request
    }

    /// ②.6 Context usage circuit-breaking — emergency trim when context
    /// exceeds hard threshold (90%), warn when approaching limit (70%).
    ///
    /// Returns `true` if the chat_request needs to be rebuilt after trimming.
    pub(crate) fn check_context_overflow_and_trim(&mut self, current_model: &str) -> bool {
        let usable = self.context_trim_budget(current_model);
        let warn_threshold = (usable as f64 * 0.70) as u64;
        let hard_threshold = (usable as f64 * 0.90) as u64;
        let current_tokens = self.session.history.token_count();

        if current_tokens > hard_threshold {
            tracing::error!(
                current_tokens,
                hard_threshold,
                usable_context = usable,
                "Context usage exceeds hard limit, emergency trimming"
            );
            let removed = self.session.history.emergency_trim();
            tracing::info!(removed, "Emergency trimmed messages for oversized context");
            removed > 0 // signal that request needs rebuild
        } else if current_tokens > warn_threshold {
            tracing::warn!(
                current_tokens,
                warn_threshold,
                usable_context = usable,
                "Context usage approaching limit"
            );
            false
        } else {
            false
        }
    }

    /// ④ Process LLM response usage — update budget, calibrate token counter,
    /// emit context usage report, and trigger compaction if needed.
    ///
    /// This is the largest inline block extracted from `execute_single_iteration`.
    pub(crate) async fn process_llm_response_usage(
        &mut self,
        response: &rollball_core::providers::traits::ChatResponse,
        current_model: &str,
    ) {
        let local_estimate = self.session.history.token_count();

        if let Some(usage) = &response.usage {
            self.session.budget_guard.update_usage(usage.total_tokens, 0.0);

            // Diagnostic: log local token estimate vs API ground truth
            tracing::info!(
                model = %current_model,
                local_estimate,
                api_prompt_tokens = usage.prompt_tokens,
                api_completion_tokens = usage.completion_tokens,
                api_total_tokens = usage.total_tokens,
                "Context usage: local estimate vs API ground truth"
            );

            // Detect providers that return prompt_tokens=0 despite having
            // non-trivial context. Skip calibration to avoid corrupting
            // the internal token counter.
            let prompt_tokens_reliable = usage.prompt_tokens > 0;
            if prompt_tokens_reliable {
                self.session.history.calibrate_from_usage(usage.prompt_tokens);
            } else {
                tracing::warn!(
                    local_estimate,
                    "API returned prompt_tokens=0 despite non-trivial context; \
                     skipping calibration and using local estimate"
                );
            }

            // Compute and emit context usage report
            let model_caps = self.get_model_capabilities(current_model);
            tracing::debug!(
                has_chunk_tx = self.core.on_chunk.is_some(),
                has_model_caps = model_caps.is_some(),
                caps_count = self.core.gateway_model_capabilities.len(),
                has_usage = true,
                "ContextUsage: checking preconditions"
            );
            if let Some(caps) = model_caps {
                let ctx_usage = if prompt_tokens_reliable {
                    crate::agent::context::compute_context_usage(caps, usage, self.core.max_output_tokens_limit)
                } else {
                    let usable = caps.effective_input_budget(self.core.max_output_tokens_limit);
                    let percent = if usable > 0 {
                        ((local_estimate as f64 / usable as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };
                    rollball_core::protocol::ContextUsageInfo {
                        context_window: caps.context_window,
                        input_tokens: local_estimate,
                        output_tokens: usage.completion_tokens,
                        total_tokens: local_estimate + usage.completion_tokens,
                        max_input_tokens: caps.max_input_tokens,
                        usable_context: usable,
                        usage_percent: percent,
                    }
                };
                tracing::debug!(
                    context_window = ctx_usage.context_window,
                    total_tokens = ctx_usage.total_tokens,
                    usage_percent = ctx_usage.usage_percent,
                    "ContextUsage: sending report"
                );
                if !self.core.try_send_chunk(ChunkEvent::ContextUsage(ctx_usage)) {
                    tracing::debug!("ContextUsage: on_chunk channel full/closed or session_id missing");
                }
            } else {
                let available: Vec<&str> = self.core.gateway_model_capabilities.keys().map(|s| s.as_str()).collect();
                let msg = format!(
                    "Model capabilities not found for '{}'. Available: {:?}. \
                     Check that the model name matches exactly (case-sensitive). \
                     Context usage display and compaction accuracy may be affected.",
                    current_model, available
                );
                tracing::warn!("ContextUsage: NOT sent — missing model capabilities for '{}'", current_model);
                let _ = self.core.try_send_chunk(ChunkEvent::Error {
                    message: msg,
                    message_id: format!("caps-missing-{}", current_model),
                });
            }

            // Check if context usage triggers compaction
            self.compact_history_if_needed(current_model, false).await;
        }
    }

    /// ⑥ Pre-trim for tool results — make room in the context window before
    /// appending tool results, which can be very large.
    ///
    /// Triggers when `current_tokens + estimated_result_tokens > 70% of usable context`.
    pub(crate) fn pre_trim_for_tool_results(
        &mut self,
        tool_results: &[String],
        current_model: &str,
    ) {
        let result_tokens_estimate: u64 = tool_results
            .iter()
            .map(|r| crate::token::count_text(r, current_model) as u64)
            .sum();
        let usable_budget = self.context_trim_budget(current_model);
        let trim_threshold = (usable_budget as f64 * 0.70) as u64;
        let current_tokens = self.session.history.token_count();
        if current_tokens.saturating_add(result_tokens_estimate) > trim_threshold {
            tracing::info!(
                current_tokens,
                result_tokens_estimate,
                trim_threshold,
                usable_budget,
                "Pre-trimming history before appending tool results"
            );
            self.trim_history_to_budget(current_model);
        }
    }
}
