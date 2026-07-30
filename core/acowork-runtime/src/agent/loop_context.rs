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

use acowork_core::providers::traits::Provider;

use crate::agent::context::count_chat_request_chars;
use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session::session_manager::RuntimeConfigOverrides;

/// ADR-032 C4b: Compression trigger mode.
///
/// - `Auto`: events (todos completion, assistant long message, persist
///   pre_trim) trigger compress_tool_results automatically. Budget fallback
///   (pre_trim_for_tool_results, trim_history_to_budget) also active.
/// - `Manual`: events are all off; user triggers via Gateway API or CLI only.
///   Budget fallback still active (avoids deadlock if user forgets to compress).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    Auto,
    Manual,
}

impl std::fmt::Display for CompressionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionMode::Auto => write!(f, "auto"),
            CompressionMode::Manual => write!(f, "manual"),
        }
    }
}

/// Default compression mode.
///
/// ADR-032 (revised): default is `Manual` so that the assistant-long-text
/// trigger and the todo_write completion trigger do **not** fire unless
/// the user explicitly opts into `Auto`. Manual mode is the safe default
/// — the user can still trigger compression through the Gateway API or
/// CLI on demand. Auto mode is an opt-in productivity shortcut.
pub const DEFAULT_COMPRESSION_MODE: CompressionMode = CompressionMode::Manual;

// ── Context compression thresholds ─────────────────────────────────────
// All percentages are relative to the **effective usable input budget**
// (`ModelCapabilitiesInfo::effective_input_budget`, i.e. context_window
// minus the reserved output space).
//
// They drive a multi-tier strategy (ADR-011):
//   WARN      → soft log / preemptive history trim before appending tool
//               results that would push the total over this threshold.
//   COMPACT   → trigger LLM-based compaction (`compact_via_llm` +
//               `replace_middle_with_summary`).
//   HARD      → force `emergency_trim` before the next LLM call (the
//               chat request is rebuilt after trimming).
//   CRITICAL  → `emergency_trim` safety net, applied directly when usage
//               jumps to ≥ this, or post-compaction if compaction alone
//               didn't bring usage back under it.
pub(crate) const CONTEXT_WARN_PERCENT: f64 = 70.0;
pub(crate) const CONTEXT_COMPACT_PERCENT: f64 = 80.0;
pub(crate) const CONTEXT_HARD_PERCENT: f64 = 90.0;
pub(crate) const CONTEXT_CRITICAL_PERCENT: f64 = 95.0;

/// Number of conversational rounds kept at the tail after LLM compaction.
/// A round begins with a User message, so this preserves the last N User
/// messages and everything after them. Consumed by both
/// [`HistoryManager::replace_middle_with_summary`] and the
/// `CompactionEventMeta` record persisted in the JSONL session log (the
/// session restorer reads the most recent such event to anchor the replay
/// window on cold-start resume).
pub(crate) const KEEP_LAST_ROUNDS: usize = 3;

// ── ADR-032 placeholder compression ──────────────────────────────────
//
// C1: the `compress_tool_results` API is in place; this commit only
// hard-codes the defaults. C4a wires these through the
// `RuntimeConfigOverrides.tool_result_keep_recent_n` config field with
// the same three-level fallback (overrides → agent_config → this default).
//
// `DEFAULT_SOFT_THRESHOLD_CHARS` — characters; an in-memory tool result
// longer than this is replaced with a placeholder. 2048 chars ≈ 512
// tokens, aligned with the typical "LLM context bloat" threshold.
//
// This constant is the **Layer-3 fallback** in the three-level
// resolution chain (ADR-032 core principle #7):
//   1. `RuntimeConfigOverrides.tool_result_soft_threshold_chars`
//      (hot-pushed via Gateway `RuntimeConfigUpdate`)
//   2. `AgentConfig.tool_result_soft_threshold_chars`
//      (persisted in `agent_config.json`, user-editable via
//      the Agent Setup panel)
///   3. `DEFAULT_SOFT_THRESHOLD_CHARS` (this constant)
///
/// Call sites should always read through
/// `AgentCore::tool_result_soft_threshold_chars()` rather than referencing
/// this constant directly — see `agent_core.rs`.
pub(crate) const DEFAULT_SOFT_THRESHOLD_CHARS: usize = 2048;

/// Number of recent tool results kept raw (not compressed) at every
/// trigger point (event / budget / restore / manual). N = 0 compresses
/// all eligible; N = 3 is the ADR-032 default (skill-typical tool call
/// depth). See `docs/adr/zh/ADR-032-context-recall.md` core principle #7.
pub(crate) const DEFAULT_KEEP_RECENT_N: usize = 3;

impl AgentLoop {
    /// Update the LLM provider at runtime (e.g., after a `ModelSwitch`
    /// message rebuilds the Provider from the global cache).
    ///
    /// The current provider_id is tracked in `SessionState.provider`,
    /// which the ModelSwitch handler updates before invoking this method.
    /// At distillation time, `resolve_distill_model` looks up the
    /// compact_model via `self.session.provider()`.
    pub fn update_provider(
        &mut self,
        new_provider: Arc<dyn Provider>,
        model: String,
        provider_id: Option<String>,
    ) {
        // `self.core` is an owned `AgentCore` (per-session clone), so we
        // can mutate it directly. The optional `provider_id` is kept as
        // a parameter for caller compatibility but is now persisted on
        // `SessionState.provider` instead of on `AgentCore`.
        self.core.update_provider(new_provider, model);
        let _ = provider_id;
    }

    /// Apply runtime config overrides from Gateway.
    pub fn apply_runtime_config(&mut self, overrides: &RuntimeConfigOverrides) {
        self.core.apply_runtime_config(overrides);
        // Sync the session's cached temperature so emit_session_state()
        // reflects the new override immediately.  Without this,
        // session.temperature() returns the old resolved value (always
        // Some), blocking the fallback chain from reaching the new
        // temperature_override.
        if overrides.temperature.is_some() {
            self.session.set_temperature(overrides.temperature);
        }

        // If context_window changed, push updated context_usage immediately
        // so the frontend status panel and context-usage popup reflect the
        // new cap without waiting for the next LLM response.
        if overrides.context_window.is_some() {
            let model_name = self.resolve_current_model(None);
            if let Some((caps, effective_window, effective_usable)) =
                self.effective_context_budget(&model_name)
            {
                let total_tokens = self.session.history.token_count();
                let percent = if effective_usable > 0 {
                    ((total_tokens as f64 / effective_usable as f64) * 100.0).min(100.0) as u8
                } else {
                    0
                };
                // Pull cumulative session totals so the frontend status
                // panel can render session-level Total Input / Total Output
                // alongside per-turn input_tokens / output_tokens.
                let (total_input, total_output) = self
                    .session
                    .conversation
                    .as_ref()
                    .and_then(|c| c.tokens())
                    .map(|t| (Some(t.total_input), Some(t.total_output)))
                    .unwrap_or((None, None));
                let ctx_info = acowork_core::protocol::ContextUsageInfo {
                    context_window: effective_window,
                    input_tokens: total_tokens,
                    output_tokens: 0,
                    total_tokens,
                    max_input_tokens: caps.max_input_tokens,
                    usable_context: effective_usable,
                    usage_percent: percent,
                    total_input_tokens: total_input,
                    total_output_tokens: total_output,
                    // ADR-028: agent-scoped cumulative tokens (snapshot of AtomicU64 counters).
                    agent_total_input_tokens: None,
                    agent_total_output_tokens: None,
                };
                tracing::info!(
                    context_window = effective_window,
                    total_tokens,
                    usage_percent = percent,
                    "Pushing immediate context_usage after context_window config change"
                );
                // ADR-028: snapshot the agent-scoped counters before pushing.
                let mut ctx_info = ctx_info;
                let (agent_in, agent_out) = self.core.agent_token_totals();
                ctx_info.agent_total_input_tokens = Some(agent_in);
                ctx_info.agent_total_output_tokens = Some(agent_out);
                let _ = self.session_core.try_send_chunk(ChunkEvent::ContextUsage(ctx_info));
            }
        }
    }

    /// Get the context window budget for history trimming.
    ///
    /// Resolves the effective budget through a per-agent cap chain
    /// (agent_config.json → manifest → DEFAULT_CONTEXT_WINDOW) then
    /// clamps to the model's actual context window capacity.
    /// Falls back to config.history_max_tokens when model capabilities
    /// are unavailable.
    pub(crate) fn context_trim_budget(&self, model_name: &str) -> u64 {
        self.core.context_trim_budget(model_name)
    }

    /// Resolve the effective (clamped) context budget for **display** and
    /// threshold checks in this file.
    ///
    /// Returns `(caps, effective_window, effective_usable)` if model
    /// capabilities are available, otherwise `None`.
    ///
    /// This is the **display-layer** budget: it considers only the runtime
    /// `context_window_override`, not `manifest_context_window` or the
    /// `DEFAULT_CONTEXT_WINDOW` fallback. The latter two feed
    /// [`Self::context_trim_budget`] (the actual trim threshold), and the
    /// divergence is intentional — the UI shows the model's full capacity
    /// by default while trimming honours user-imposed caps.
    pub(crate) fn effective_context_budget(
        &self,
        model_name: &str,
    ) -> Option<(acowork_core::protocol::ModelCapabilitiesInfo, u64, u64)> {
        let caps = self.core.get_model_capabilities(model_name)?;
        let max_output_limit = self.core.max_output_tokens_limit_for_model(model_name);
        let usable = caps.effective_input_budget(max_output_limit);
        let effective_window = match self.core.context_window_override {
            Some(0) | None => caps.context_window,
            Some(cap) => cap.min(caps.context_window),
        };
        Some((caps, effective_window, usable.min(effective_window)))
    }

    /// Trim history to fit within the context window budget.
    ///
    /// The budget comes from [`context_trim_budget`] →
    /// [`ModelCapabilitiesInfo::effective_input_budget`], which already
    /// reserves output space (capped at `max_output_tokens_limit`, default 32K).
    /// No additional margin is applied here — [`compact_history_if_needed`]
    /// provides early warning at 80% usage.
    ///
    /// **ADR-032 (revised)**: this budget-fallback path is intentionally a
    /// **token-only** fallback. It performs FIFO trim + emergency trim only;
    /// it does NOT call `compress_tool_results`. Placeholder compression is
    /// triggered by event-style signals (assistant long message / todo_write
    /// completion in Auto mode, or explicit user commands in Manual mode) —
    /// see ADR-032 for the full trigger matrix. Mixing budget-fallback into
    /// the event-driven trigger path causes the
    /// recall → compress → recall loop (the budget fall-back fires after
    /// every tool result append, re-compressing any content that `context_recall`
    /// just restored, forcing the LLM to recall again).
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

        // NOTE: placeholder compression is intentionally NOT performed here.
        // See method-level doc above.
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

        // Path 1: resolve compact_model from current provider (in-memory).
        // The current provider id lives on the per-session SessionState
        // (set by ModelSwitch handler) — there is no global "current
        // provider" on AgentCore anymore.
        let compact_model: Option<String> = self
            .session
            .provider()
            .and_then(|pid| self.core.provider_compact_models.get(pid))
            .and_then(|cm| cm.clone());
        if let Some(ref compact_model) = compact_model {
            if let Some(cap) = self.core.get_model_capabilities(compact_model) {
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
    /// - `CONTEXT_CRITICAL_PERCENT` usage → emergency trim (safety net)
    ///
    /// When `force` is true (manual trigger from user), the 80% threshold is
    /// bypassed and compaction proceeds regardless of current usage percentage.
    ///
    /// Called after each LLM response (force=false) and on manual user trigger
    /// (force=true via `SessionMessage::CompactContext`).
    pub(crate) async fn compact_history_if_needed(&mut self, model_name: &str, force: bool) {
        let budget = self.context_trim_budget(model_name);
        let current_tokens = self.session.history.token_count();

        if budget == 0 {
            return;
        }

        let usage_percent = (current_tokens as f64 / budget as f64) * 100.0;

        // Stage 2: CONTEXT_COMPACT_PERCENT → LLM-based compaction
        // (or force=true bypasses threshold).
        if force || usage_percent >= CONTEXT_COMPACT_PERCENT {
            tracing::info!(
                usage_percent = ?usage_percent,
                current_tokens,
                budget,
                force,
                "Triggering LLM compaction"
            );

            // Notify frontend that compaction has started (both manual and auto paths).
            let _ = self.session_core.try_send_chunk(ChunkEvent::CompactingStarted);

            // Build combined text from history for model-aware token counting.
            let combined_text: String =
                self.session
                    .history
                    .messages()
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
                .compact_via_llm(
                    provider.as_ref(),
                    &compact_model,
                    system_prompt,
                    self.session.identity_context(),
                )
                .await
            {
                Ok((summary, usage)) => {
                    // ADR-027 (revised for compaction): record raw Provider
                    // usage from the compaction summary call into the session
                    // token accumulator, but **do not** overwrite `last_input`
                    // with the summary LLM's `prompt_tokens`. The summary LLM
                    // was given the **full pre-compaction history** as input,
                    // so its prompt size is unrepresentative of the
                    // post-compaction session state — overwriting `last_input`
                    // would inflate the displayed context usage until the
                    // next main-dialog LLM call recalibrates it.
                    //
                    // `accumulate_compaction_usage` preserves `last_input` /
                    // `last_output` and only updates cumulative totals.
                    // `set_history_anchor` (called below after
                    // `replace_middle_with_summary`) re-anchors `last_input`
                    // to the post-compaction `history.token_count()`.
                    if let Some(ref conversation) = self.session.conversation {
                        conversation.accumulate_compaction_usage(&usage);
                    }
                    // ADR-028: also feed the agent-scoped counters so the
                    // agent-total line in Results Panel updates even if
                    // the session's persisted meta is still on disk.
                    self.core.accumulate_llm_usage(&usage);
                    let stripped = crate::episode_distill::strip_metadata_blocks(&summary);
                    let removed = self
                        .session
                        .history
                        .replace_middle_with_summary(&stripped, KEEP_LAST_ROUNDS);

                    // Recompute usage after compaction (used both for the
                    // JSONL event payload and the stage-3 emergency check).
                    let new_tokens = self.session.history.token_count();
                    let new_usage = if budget > 0 {
                        (new_tokens as f64 / budget as f64) * 100.0
                    } else {
                        0.0
                    };

                    // Anchor `last_input` to the post-compaction history size
                    // so the next `emit_session_state()` push reports the new
                    // (smaller) `usage_percent` **immediately**, without
                    // waiting for the next main-dialog LLM call. The value is
                    // heuristic — it will be overwritten by the next
                    // `accumulate_llm_usage()` with the API's authoritative
                    // `prompt_tokens` — but the heuristic is good enough for
                    // the immediate StatusBar refresh and prevents the
                    // "stale 70% before, drops to 20% after next message"
                    // UX glitch.
                    //
                    // The anchor must run AFTER `replace_middle_with_summary`
                    // (so `history.token_count()` reflects the new state) and
                    // BEFORE `append_compaction_event` (so the JSONL event
                    // carries the post-anchor token state).
                    if let Some(ref conversation) = self.session.conversation {
                        conversation.set_history_anchor(new_tokens);

                        let meta = crate::conversation::CompactionEventMeta {
                            // Range tracking is best-effort; we don't currently
                            // know the precise from/to entry ids without
                            // threading them through. Leaving them empty is
                            // acceptable — the restorer only needs the event's
                            // *position* in the log, not the id range.
                            compacted_from_id: String::new(),
                            compacted_to_id: String::new(),
                            keep_last_rounds: KEEP_LAST_ROUNDS,
                            model: compact_model.clone(),
                            before_tokens: current_tokens,
                            after_tokens: new_tokens,
                        };
                        conversation.append_compaction_event(&stripped, meta);
                    }

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
                    )
                    .await;

                    // Mark session as compacted (zero new messages since compaction)
                    self.session.is_compacted = true;

                    // Path C: Run generalization after successful compaction.
                    // Scans unconsolidated episodes for behavior patterns and
                    // creates/boosts ProceduralNodes (rule-based only, no LLM).
                    self.run_generalization_if_possible().await;

                    // P2-1: Self-evaluate skill performance after generalization.
                    // Checks ProceduralNode success/fail rates and creates
                    // Limitation autobiographical nodes for low-performing skills.
                    self.self_evaluate_skill_performance();

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
                    if new_usage >= CONTEXT_CRITICAL_PERCENT {
                        let em_removed = self.session.history.emergency_trim();
                        tracing::warn!(
                            em_removed,
                            after_usage = ?new_usage,
                            "Emergency trim performed after compaction (still >= CONTEXT_CRITICAL_PERCENT)"
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
                    // ADR-032 (revised): placeholder compression is intentionally
                    // NOT performed here either. The fallback path is token-only,
                    // matching the behavior of `trim_history_to_budget`. See the
                    // docstring on that method for the full rationale.
                }
            }

            // Notify frontend that compaction has finished, so it can clear
            // the "compacting..." indicator (both success and error paths).
            // Also send updated context usage so the frontend shows the new
            // token count and percentage after compaction.
            let _ = self.session_core.try_send_chunk(ChunkEvent::CompactingEnded);

            // Re-emit the runtime session-state snapshot now that
            // `last_input` reflects the post-compaction history size (via
            // `set_history_anchor` above). Without this call, the
            // `SessionStateChanged` event published earlier in this flow
            // (by `accumulate_compaction_usage → notify_state_change`)
            // carries a stale, inflated `usage_percent` that contradicts
            // the standalone `ContextUsage` event below. Frontends that
            // key off `SessionStateChanged` (e.g. for the retained
            // `sessions/{sid}/state` topic) would otherwise show the old
            // percentage until the next main-dialog LLM call recalibrates.
            //
            // The standalone `ContextUsage(ctx_info)` push below carries
            // the same numeric value computed here from
            // `history.token_count()`, so the two channels stay consistent.
            self.emit_session_state();

            // Compute and send updated context usage after compaction.
            if let Some((caps, effective_window, effective_usable)) =
                self.effective_context_budget(model_name)
            {
                let total_tokens = self.session.history.token_count();
                let usage_percent = if effective_usable > 0 {
                    ((total_tokens as f64 / effective_usable as f64) * 100.0).min(100.0) as u8
                } else {
                    0
                };
                // Pull cumulative session totals for the post-compaction
                // snapshot so the frontend status panel can update both
                // per-turn and cumulative fields in one push.
                let (total_input, total_output) = self
                    .session
                    .conversation
                    .as_ref()
                    .and_then(|c| c.tokens())
                    .map(|t| (Some(t.total_input), Some(t.total_output)))
                    .unwrap_or((None, None));
                let ctx_info = acowork_core::protocol::ContextUsageInfo {
                    context_window: effective_window,
                    input_tokens: total_tokens,
                    output_tokens: 0,
                    total_tokens,
                    max_input_tokens: caps.max_input_tokens,
                    usable_context: effective_usable,
                    usage_percent,
                    total_input_tokens: total_input,
                    total_output_tokens: total_output,
                    // ADR-028: agent-scoped cumulative tokens (snapshot of AtomicU64 counters).
                    agent_total_input_tokens: None,
                    agent_total_output_tokens: None,
                };
                // ADR-028: snapshot the agent-scoped counters before pushing.
                let mut ctx_info = ctx_info;
                let (agent_in, agent_out) = self.core.agent_token_totals();
                ctx_info.agent_total_input_tokens = Some(agent_in);
                ctx_info.agent_total_output_tokens = Some(agent_out);
                let _ = self.session_core.try_send_chunk(ChunkEvent::ContextUsage(ctx_info));
            }
        } else if usage_percent >= CONTEXT_CRITICAL_PERCENT {
            // Stage 3: emergency trim without attempting compaction
            // (when usage jumps directly to >= CONTEXT_CRITICAL_PERCENT)
            let removed = self.session.history.emergency_trim();
            tracing::warn!(
                removed,
                usage_percent = ?usage_percent,
                current_tokens,
                budget,
                "Emergency trim performed (usage >= CONTEXT_CRITICAL_PERCENT)"
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
        use acowork_core::providers::traits::{ChatMessage, MessageRole};

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
    ) -> acowork_core::providers::traits::ChatRequest {
        // Inject current todo list into system prompt before building
        context_builder.set_todo_context(self.session.format_todos());
        let caps = self.get_model_capabilities(current_model);
        let max_output_limit = self.core.max_output_tokens_limit_for_model(current_model);

        // Resolve reasoning_effort: read from per-session state (initialized from
        // the three-level priority chain in build_initial_session_state).
        // None  = provider does not support thinking control (frontend hides control).
        // Some(Auto) = provider supports it but no parameter sent to LLM (model decides).
        // Some(Low/Medium/High/...) = explicit level sent to LLM.
        // The fallback to caps.default_reasoning_effort is kept for safety in case
        // the session state was not initialized yet (e.g. direct AgentLoop usage).
        let reasoning_effort = self
            .session
            .reasoning_effort()
            .cloned()
            .or_else(|| {
                caps
                    .as_ref()
                    .and_then(|c| c.default_reasoning_effort.as_deref())
                    .and_then(acowork_core::providers::traits::ReasoningEffort::from_str_loose)
            });
        context_builder.set_reasoning_effort(reasoning_effort.clone());
        // Cache for emergency trim retry in call_llm_streaming_inner()
        // where context_builder is immutable.
        self.last_reasoning_effort = reasoning_effort;

        // Resolve thinking_mode (Anthropic: "extended" vs "adaptive")
        let thinking_mode = caps
            .as_ref()
            .and_then(|c| c.thinking_mode.clone());
        context_builder.set_thinking_mode(thinking_mode.clone());
        self.last_thinking_mode = thinking_mode;

        // Resolve temperature via the per-agent chain:
        //   agent_config.json (Layer 1) → manifest default (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).
        // Always set a concrete value on the builder so the ChatRequest reflects what
        // the model will actually receive, and so the value shown in the status panel
        // matches the request payload.
        let temperature = self
            .session
            .temperature()
            .or(self.core.temperature_override)
            .or(self.core.manifest_temperature)
            .unwrap_or(crate::config::DEFAULT_TEMPERATURE);
        context_builder.set_temperature(Some(temperature));

        let mut chat_request = context_builder.build(
            &self.core.manifest,
            &self.session.history,
            caps.as_ref(),
            max_output_limit,
        );

        tracing::info!(
            request_messages_count = chat_request.messages.len(),
            request_model = %chat_request.model,
            request_max_tokens = ?chat_request.max_tokens,
            request_tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            history_tokens = self.session.history.token_count(),
            "Built chat request for LLM (after preemptive trim)"
        );

        // Inject MCP server tool definitions into the LLM request.
        //
        // `chat_request.tools` contains built-in tool definitions from
        // `tool_definitions` (set at startup, does not include MCP tools).
        // MCP server tools live separately in `self.core.mcp_tools` and must
        // be injected here before each LLM call.
        //
        // We iterate `self.core.mcp_tools` directly — NOT `self.core.all_tools`
        // filtered by name prefix — because `all_tools` mixes built-in tools
        // (mcp_install, mcp_uninstall) with MCP server tools, and filtering by
        // "mcp_" prefix would re-inject built-in tools and cause
        // "Tool names must be unique" 400 errors.
        if let Some(ref mut tools) = chat_request.tools
            && let Some(ref mcp_tools) = self.core.mcp_tools
        {
            for tool in mcp_tools {
                let spec = tool.spec();
                let val = serde_json::to_value(&spec).unwrap_or_default();
                tools.push(val);
            }
        }

        // Compute total input chars for next round's token ratio calibration.
        self.last_input_chars = count_chat_request_chars(&chat_request);

        // Inject transient tool results from the previous iteration
        // (ADR-032 C3a). These are one-shot messages (e.g., non-context_recall
        // transient results) that are visible to the LLM for one request only.
        if !self.pending_transient_tool_msgs.is_empty() {
            let count = self.pending_transient_tool_msgs.len();
            chat_request.messages.append(&mut self.pending_transient_tool_msgs);
            debug_assert!(self.pending_transient_tool_msgs.is_empty());
            tracing::debug!(count, "Injected transient tool results into chat request");
        }

        chat_request
    }

    /// ②.6 Context usage circuit-breaking — emergency trim when context
    /// exceeds hard threshold (90%), warn when approaching limit (70%).
    ///
    /// Returns `true` if the chat_request needs to be rebuilt after trimming.
    pub(crate) fn check_context_overflow_and_trim(&mut self, current_model: &str) -> bool {
        let usable = self.context_trim_budget(current_model);
        let warn_threshold = (usable as f64 * (CONTEXT_WARN_PERCENT / 100.0)) as u64;
        let hard_threshold = (usable as f64 * (CONTEXT_HARD_PERCENT / 100.0)) as u64;
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
        response: &acowork_core::providers::traits::ChatResponse,
        current_model: &str,
    ) {
        let local_estimate = self.session.history.token_count();

        if let Some(usage) = &response.usage {
            self.session
                .budget_guard
                .update_usage(usage.total_tokens, 0.0);

            // Diagnostic: log local token estimate vs API ground truth
            tracing::info!(
                model = %current_model,
                local_estimate,
                api_prompt_tokens = usage.prompt_tokens,
                api_completion_tokens = usage.completion_tokens,
                api_reasoning_tokens = usage.reasoning_tokens,
                api_content_tokens = usage.completion_tokens.saturating_sub(usage.reasoning_tokens),
                api_total_tokens = usage.total_tokens,
                "Context usage: local estimate vs API ground truth"
            );

            // Detect providers that return prompt_tokens=0 despite having
            // non-trivial context. Skip calibration to avoid corrupting
            // the internal token counter.
            let prompt_tokens_reliable = usage.prompt_tokens > 0;
            if prompt_tokens_reliable {
                self.session
                    .history
                    .calibrate_from_usage(usage.prompt_tokens, self.last_input_chars);

                // Persist the calibrated ratio into SessionState so the
                // next emit_session_state checkpoint will pick it up.
                if let Some(ratio) = self.session.history.model_ratio() {
                    self.session.set_model_ratio(ratio);
                }
            } else {
                tracing::warn!(
                    local_estimate,
                    "API returned prompt_tokens=0 despite non-trivial context; \
                     skipping calibration and using local estimate"
                );
            }

            // Compute and emit context usage report
            let model_caps = self.get_model_capabilities(current_model);
            let max_output_limit = self.core.max_output_tokens_limit_for_model(current_model);
            tracing::debug!(
                has_chunk_tx = self.session_core.chunk_tx.is_some(),
                has_model_caps = model_caps.is_some(),
                has_usage = true,
                "ContextUsage: checking preconditions"
            );
            if let Some(caps) = model_caps {
                let ctx_usage = if prompt_tokens_reliable {
                    crate::agent::context::compute_context_usage(
                        &caps,
                        usage,
                        max_output_limit,
                        self.core.context_window_override,
                    )
                } else {
                    // Re-resolve via the helper: model_caps in scope and the
                    // helper agree (same source), so this is infallible here.
                    let (caps, effective_window, effective_usable) = self
                        .effective_context_budget(current_model)
                        .expect("model_caps already verified Some above");
                    let percent = if effective_usable > 0 {
                        ((local_estimate as f64 / effective_usable as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };
                    acowork_core::protocol::ContextUsageInfo {
                        context_window: effective_window,
                        input_tokens: local_estimate,
                        output_tokens: usage.completion_tokens,
                        total_tokens: local_estimate + usage.completion_tokens,
                        max_input_tokens: caps.max_input_tokens,
                        usable_context: effective_usable,
                        usage_percent: percent,
                        total_input_tokens: None,
                        total_output_tokens: None,
                        // ADR-028: agent-scoped cumulative tokens (snapshot of AtomicU64 counters).
                        agent_total_input_tokens: None,
                        agent_total_output_tokens: None,
                    }
                };
                tracing::debug!(
                    context_window = ctx_usage.context_window,
                    total_tokens = ctx_usage.total_tokens,
                    usage_percent = ctx_usage.usage_percent,
                    "ContextUsage: sending report"
                );

                // Persist the raw Provider-reported usage counts so the
                // frontend context-usage indicator can be restored on
                // session resume. ADR-027 deliberately uses `usage.*`
                // (raw Provider values) rather than `ctx_usage.*` so the
                // persisted snapshot faithfully reflects what the Provider
                // returned — even when `prompt_tokens_reliable == false`
                // and the frontend display falls back to the local
                // tokenizer estimate. The "宁可 miss 也不估计" policy
                // prefers a raw zero over a local estimate in the
                // accumulator.
                //
                // After accumulation, the SessionTokens cumulative totals
                // (`total_input`, `total_output`) reflect this LLM call,
                // so we patch the just-computed `ctx_usage` with them
                // before pushing to the frontend. This lets the status
                // panel show session-level cumulative figures alongside
                // per-turn input_tokens / output_tokens.
                let mut ctx_usage = ctx_usage;
                if let Some(ref conv) = self.session.conversation {
                    conv.accumulate_llm_usage(usage);
                    if let Some(t) = conv.tokens() {
                        ctx_usage.total_input_tokens = Some(t.total_input);
                        ctx_usage.total_output_tokens = Some(t.total_output);
                    }
                }
                // ADR-028: feed the agent-scoped counters and snapshot them
                // into the push payload so the frontend's Results Panel can
                // show a "Agent total" line. Live in this path is the
                // primary data source; the session-list response carries a
                // fallback copy (see `handle_list_sessions`).
                self.core.accumulate_llm_usage(usage);
                let (agent_in, agent_out) = self.core.agent_token_totals();
                ctx_usage.agent_total_input_tokens = Some(agent_in);
                ctx_usage.agent_total_output_tokens = Some(agent_out);

                if !self
                    .session_core
                    .try_send_chunk(ChunkEvent::ContextUsage(ctx_usage))
                {
                    tracing::debug!(
                        "ContextUsage: chunk channel full/closed or session_id missing"
                    );
                }
            } else {
                let available: Vec<String> = {
                    let list = self.core.global_provider_list.read().unwrap();
                    list.iter()
                        .flat_map(|p| p.models.iter().map(|m| m.id.clone()))
                        .collect()
                };
                let msg = format!(
                    "Model capabilities not found for '{}'. Available: {:?}. \
                     Check that the model name matches exactly (case-sensitive). \
                     Context usage display and compaction accuracy may be affected.",
                    current_model, available
                );
                tracing::warn!(
                    "ContextUsage: NOT sent — missing model capabilities for '{}'",
                    current_model
                );
                let _ = self.session_core.try_send_chunk(ChunkEvent::Error {
                    user_message: msg,
                    detail: String::new(),
                    error_type: "ContextOverflow".to_string(),
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
        let trim_threshold = (usable_budget as f64 * (CONTEXT_WARN_PERCENT / 100.0)) as u64;
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

    /// ⑥.5 Context-aware tool result trimming — truncate individual tool
    /// results so they don't overflow the context window when appended.
    ///
    /// After [`pre_trim_for_tool_results`] trims history, this ensures the
    /// tool results themselves fit within the remaining context budget.
    /// Each result gets a proportional share of the remaining space, and
    /// truncated results carry a detailed marker so the LLM can adapt its
    /// strategy (narrower search, pagination, etc.).
    ///
    /// Returns the number of results that were truncated.
    pub(crate) fn trim_tool_results_for_context(
        &self,
        tool_results: &mut [String],
        current_model: &str,
    ) -> usize {
        let usable_budget = self.context_trim_budget(current_model);
        let current_tokens = self.session.history.token_count();

        // Safety margin: reserve 15% of the usable budget for the LLM's
        // next response (output tokens), tool call metadata, and message
        // serialisation overhead. Without this, the trimmed results may
        // fit the remaining space but the follow-up LLM response will
        // immediately overflow again.
        let safe_budget = (usable_budget as f64 * 0.85) as u64;
        let remaining = safe_budget.saturating_sub(current_tokens);

        if remaining == 0 || tool_results.is_empty() {
            return 0;
        }

        // Count tokens per result
        let result_tokens: Vec<u64> = tool_results
            .iter()
            .map(|r| crate::token::count_text(r, current_model) as u64)
            .collect();
        let total_result_tokens: u64 = result_tokens.iter().sum();

        if total_result_tokens <= remaining {
            return 0; // Everything fits
        }

        let n = tool_results.len() as u64;
        // Minimum budget per result: 256 tokens ensures the LLM still
        // gets meaningful output. Below this, every result would be
        // reduced to just the truncation marker, which is unhelpful.
        let min_per_result: u64 = 256;
        let per_result_budget = (remaining / n).max(min_per_result);

        tracing::warn!(
            current_tokens,
            total_result_tokens,
            remaining,
            per_result_budget,
            result_count = n,
            usable_budget,
            "Tool results exceed remaining context budget — truncating"
        );

        let mut truncated = 0;
        for (i, result) in tool_results.iter_mut().enumerate() {
            let orig_tokens = result_tokens[i];
            if orig_tokens <= per_result_budget {
                continue;
            }

            let original_bytes = result.len();
            // Convert token budget to char budget (≈ 4 chars/token for CJK, ≈ 3 for ASCII).
            // Use 3.5 as a middle-ground multiplier and add 10% padding for safety.
            let max_chars = ((per_result_budget as f64) * 3.5 * 1.1) as usize;

            // Find a UTF-8 safe cut point within budget
            let mut cut = max_chars.min(result.len());
            while cut > 0 && !result.is_char_boundary(cut) {
                cut -= 1;
            }

            // Try to cut at a newline for readability
            if let Some(nl_pos) = result[..cut].rfind('\n').filter(|&p| p > cut / 2) {
                cut = nl_pos;
            }

            let kept = &result[..cut];
            let dropped_tokens = orig_tokens.saturating_sub(per_result_budget);

            let truncation_marker = format!(
                "\n\n[RESULT TRUNCATED by context budget: original output was {} bytes \
                 (~{} tokens), but only {per_result_budget} tokens of context budget remain \
                 per result (total remaining: {remaining} tokens across {n} result(s)). \
                 ~{dropped_tokens} tokens of this output were dropped. \
                 SUGGESTION: re-run with narrower parameters (pipe through 'head -N' or \
                 'tail -N' for pagination, use tighter grep patterns to reduce matches, \
                 or search fewer files/directories) to get a complete result.]",
                original_bytes,
                orig_tokens,
            );

            *result = format!("{kept}{truncation_marker}");
            truncated += 1;

            tracing::warn!(
                i,
                original_bytes,
                orig_tokens,
                kept_bytes = cut,
                per_result_budget,
                "Tool result #{i} truncated to fit context budget"
            );
        }

        truncated
    }
}
