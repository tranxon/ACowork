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

// ── ADR-056: Distillation resolution types ────────────────────────

/// ADR-056: Resolution tier for a chosen distillation target.
///
/// Logged on every compaction call so it's easy to verify in production
/// which fallback chain fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DistillTier {
    /// Level 1: `AgentCore.default_compact_model` — user's global pick
    /// across all configured providers.
    GlobalDefault,
    /// Level 2: `ProviderListItem.compact_model` of the session's current
    /// provider (the pre-ADR-056 behaviour).
    ProviderCompact,
    /// Level 3: the session's current chat model.
    CurrentChat,
}

/// ADR-056: Result of resolving the (provider, model) target for a
/// distillation / compaction call. Carries the tier so the caller can
/// log it and pick the right `Provider` instance (cross-provider
/// distillation requires `SessionCore::build_provider_for`, while
/// `ProviderCompact` / `CurrentChat` can reuse `self.core.provider`).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDistill {
    pub provider_id: String,
    pub model_id: String,
    pub tier: DistillTier,
}

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
    /// **ADR-052**: this budget-fallback path is intentionally a
    /// **token-only** fallback. It performs FIFO trim + emergency trim only;
    /// it does NOT call any compression/placeholder logic. Placeholder
    /// compression is now LLM-initiated via the `context_abandon` tool;
    /// budget fallback must not be conflated with the tool-compression
    /// path. (Historically this was a fix for the
    /// recall → compress → recall loop in ADR-032.)
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
    pub(crate) fn resolve_distill_model(&self, content_text: &str) -> ResolvedDistill {
        let current_model = self.resolve_current_model(None);
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();

        // Choose the probe model for token estimation (best effort — see doc).
        let probe_model = self
            .core
            .default_compact_model
            .as_ref()
            .map(|(_, m)| m.clone())
            .or_else(|| {
                self.core
                    .provider_compact_models
                    .get(&session_pid)
                    .and_then(|cm| cm.clone())
            })
            .unwrap_or_else(|| current_model.clone());
        let estimated_tokens = crate::token::count_text(content_text, &probe_model) as u64;

        // ── Level 1: global default compact model ──────────────────────
        if let Some((pid, mid)) = self.core.default_compact_model.clone() {
            if !self.core.is_default_compact_provider_available() {
                tracing::warn!(
                    provider_id = %pid,
                    model_id = %mid,
                    "Global default compact model provider unavailable, falling back to Level 2"
                );
            } else if let Some(cap) = self.core.get_model_capabilities(&mid) {
                if cap.context_window < estimated_tokens {
                    tracing::warn!(
                        provider_id = %pid,
                        model_id = %mid,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Global default compact model context_window too small, falling back to Level 2"
                    );
                } else {
                    tracing::info!(
                        provider_id = %pid,
                        model_id = %mid,
                        context_window = cap.context_window,
                        estimated_tokens,
                        tier = "global_default",
                        "Using global default compact model for distillation"
                    );
                    return ResolvedDistill {
                        provider_id: pid,
                        model_id: mid,
                        tier: DistillTier::GlobalDefault,
                    };
                }
            } else {
                tracing::warn!(
                    provider_id = %pid,
                    model_id = %mid,
                    "Global default compact model not found in capabilities, falling back to Level 2"
                );
            }
        }

        // ── Level 2: session provider's compact_model ──────────────────
        if !session_pid.is_empty()
            && let Some(Some(compact)) = self.core.provider_compact_models.get(&session_pid)
        {
            if let Some(cap) = self.core.get_model_capabilities(compact) {
                if cap.context_window < estimated_tokens {
                    tracing::warn!(
                        provider_id = %session_pid,
                        model_id = %compact,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Provider compact model context_window too small, falling back to Level 3"
                    );
                } else {
                    tracing::info!(
                        provider_id = %session_pid,
                        model_id = %compact,
                        context_window = cap.context_window,
                        estimated_tokens,
                        tier = "provider_compact",
                        "Using provider's compact model for distillation"
                    );
                    return ResolvedDistill {
                        provider_id: session_pid.clone(),
                        model_id: compact.clone(),
                        tier: DistillTier::ProviderCompact,
                    };
                }
            } else {
                tracing::warn!(
                    provider_id = %session_pid,
                    model_id = %compact,
                    "Provider compact model not found in capabilities, falling back to Level 3"
                );
            }
        }

        // ── Level 3: current chat model ────────────────────────────────
        tracing::info!(
            provider_id = %session_pid,
            model_id = %current_model,
            estimated_tokens,
            tier = "current_chat",
            "Compact model not available or insufficient, using current model for distillation"
        );
        ResolvedDistill {
            provider_id: session_pid,
            model_id: current_model,
            tier: DistillTier::CurrentChat,
        }
    }

    /// ADR-056 follow-up: Return the ordered list of usable distillation
    /// targets for [`Self::compact_history_if_needed`].
    ///
    /// Differences from [`Self::resolve_distill_model`]:
    /// - Returns **all** candidates the current state can reach (up to three),
    ///   in priority order: `[GlobalDefault?, ProviderCompact?, CurrentChat]`.
    /// - `CurrentChat` is always present as the last-resort fallback so the
    ///   caller can guarantee at least one `distill_provider()` target.
    /// - Does **not** log per-tier "Using X for distillation" messages; the
    ///   caller logs the chosen tier after the LLM call returns.
    ///
    /// This is the input for the *call-phase* fallback chain: when a
    /// higher tier's LLM call fails (network, 4xx, 5xx, parse error), the
    /// caller iterates through `targets` until one succeeds, instead of
    /// falling through to `trim_fifo` + `emergency_trim` (which would
    /// destroy early context unconditionally).
    pub(crate) fn resolve_distill_targets(&self) -> Vec<ResolvedDistill> {
        // Reuse `resolve_distill_model` for the top-priority target — it
        // already encodes the same selection logic as the single-tier
        // resolver, including context-window and capability checks.
        let top = self.resolve_distill_model("");
        let mut out = vec![top.clone()];

        // Append any tiers the top-tier selection skipped over, in priority
        // order. We do this by re-evaluating each tier in isolation against
        // the current `AgentCore` state.
        if !matches!(top.tier, DistillTier::GlobalDefault)
            && let Some(t) = self.try_global_default_target()
            && !out.iter().any(|r| r.tier == t.tier)
        {
            out.insert(0, t);
        }
        if !matches!(top.tier, DistillTier::ProviderCompact)
            && let Some(t) = self.try_provider_compact_target()
            && !out.iter().any(|r| r.tier == t.tier)
        {
            // Insert after GlobalDefault (if any), before top.
            let pos = out
                .iter()
                .position(|r| !matches!(r.tier, DistillTier::GlobalDefault))
                .unwrap_or(out.len());
            out.insert(pos, t);
        }
        // CurrentChat is the universal last-resort: always appended unless
        // `resolve_distill_model` already chose it as the top tier. This
        // guarantees the caller has at least one fallback even when both
        // GlobalDefault and ProviderCompact are absent.
        if !matches!(top.tier, DistillTier::CurrentChat) {
            let current_model = self.resolve_current_model(None);
            let session_pid = self
                .session
                .provider()
                .map(str::to_string)
                .unwrap_or_default();
            let level3 = ResolvedDistill {
                provider_id: session_pid,
                model_id: current_model,
                tier: DistillTier::CurrentChat,
            };
            if !out.iter().any(|r| r.tier == DistillTier::CurrentChat) {
                out.push(level3);
            }
        }

        // De-duplicate by (provider_id, model_id) preserving order — guards
        // against the degenerate case where two tiers resolve to the same
        // (provider, model) pair (e.g. GlobalDefault == CurrentChat).
        let mut deduped: Vec<ResolvedDistill> = Vec::with_capacity(out.len());
        for r in out {
            if !deduped
                .iter()
                .any(|d| d.provider_id == r.provider_id && d.model_id == r.model_id)
            {
                deduped.push(r);
            }
        }
        deduped
    }

    /// Probe whether the global default compact model can serve as a
    /// distillation target right now. Mirrors the selection logic in
    /// [`Self::resolve_distill_model`] but returns `None` instead of
    /// falling through.
    fn try_global_default_target(&self) -> Option<ResolvedDistill> {
        let (pid, mid) = self.core.default_compact_model.as_ref()?.clone();
        if !self.core.is_default_compact_provider_available() {
            return None;
        }
        // No token-estimated context_window check here: if Level 1's
        // context_window is too small for the actual content, the LLM
        // call will still succeed (just produce a short summary); the
        // *call-phase* fallback only kicks in on call failure. The
        // selection phase already rejected this candidate via
        // `resolve_distill_model` when context_window < estimated_tokens,
        // so re-checking here would only produce a different answer than
        // the top-tier resolver — keep them aligned.
        let _ = self.core.get_model_capabilities(&mid);
        Some(ResolvedDistill {
            provider_id: pid,
            model_id: mid,
            tier: DistillTier::GlobalDefault,
        })
    }

    /// Probe whether the session provider's `compact_model` is usable.
    fn try_provider_compact_target(&self) -> Option<ResolvedDistill> {
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();
        if session_pid.is_empty() {
            return None;
        }
        let compact = self
            .core
            .provider_compact_models
            .get(&session_pid)
            .and_then(|cm| cm.clone())?;
        Some(ResolvedDistill {
            provider_id: session_pid,
            model_id: compact,
            tier: DistillTier::ProviderCompact,
        })
    }

    /// ADR-056: Pick the `Provider` instance to use for a distillation /
    /// compaction LLM call given a resolved target, and return the
    /// `(provider, model_id, tier)` triple the caller must actually use.
    ///
    /// Levels 2 & 3 reuse the session's current provider (which matches the
    /// resolved model). Level 1 may live on a *different* provider entirely,
    /// so it is rebuilt via `SessionCore::build_provider_for` with the
    /// target's base_url + api_key from the provider list / key vault.
    ///
    /// When the target provider cannot be rebuilt (removed from the list
    /// between resolve and call), both the provider **and** the model are
    /// demoted to the session's current chat model (Level 3) — never use a
    /// cross-provider model_id with the session provider, the API would
    /// reject it.
    pub(crate) fn distill_provider(
        &self,
        resolved: &ResolvedDistill,
    ) -> (Arc<dyn Provider>, String, DistillTier) {
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();

        let same_provider =
            resolved.provider_id == session_pid || resolved.provider_id.is_empty();
        if same_provider {
            return (
                self.core.provider.clone(),
                resolved.model_id.clone(),
                resolved.tier,
            );
        }

        match self.session_core.build_provider_for(
            &resolved.provider_id,
            &self.core.config,
            &self.core.global_provider_list,
            &self.core.provider_key_vault,
            self.core.compat_cache.as_ref(),
        ) {
            Some(p) => {
                tracing::info!(
                    target_provider = %resolved.provider_id,
                    target_model = %resolved.model_id,
                    session_provider = %session_pid,
                    tier = ?resolved.tier,
                    "ADR-056: rebuilt provider for cross-provider distillation"
                );
                (p, resolved.model_id.clone(), resolved.tier)
            }
            None => {
                tracing::warn!(
                    target_provider = %resolved.provider_id,
                    target_model = %resolved.model_id,
                    "ADR-056: failed to rebuild provider for cross-provider distillation, demoting to session provider + current chat model"
                );
                (
                    self.core.provider.clone(),
                    self.resolve_current_model(None),
                    DistillTier::CurrentChat,
                )
            }
        }
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
            let _resolved_distill = self.resolve_distill_model(&combined_text);
            // Compaction prompt resolution chain:
            //   1. AgentCore.compaction_prompt — package-declared
            //      `prompts/summary.md` (per-agent summarization rules).
            //   2. Built-in COMPACTION_SYSTEM_PROMPT — universal fallback.
            // The main-dialog `system_prompt_override` is deliberately NOT
            // consulted: it overrides the dialog identity, while compaction
            // is a summarization task with its own directive (see
            // `AgentCore.compaction_prompt` doc).
            let system_prompt = self
                .core
                .compaction_prompt
                .as_deref()
                .unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT);

            // ADR-056 follow-up — call-phase fallback: try the resolved
            // target first; on LLM call failure (network, 4xx, 5xx, parse
            // error), step down to the next tier in `resolve_distill_targets()`
            // order. The previous behaviour was a single attempt + unconditional
            // `trim_fifo` + `emergency_trim` on error — which silently destroys
            // early history when the global-default provider is down. The
            // three-tier fallback chain mirrors the chain the *selection* phase
            // already uses (ADR-056 §3.2), so the user only loses history when
            // **all three** distillation targets fail.
            let memory_provider = self.core.memory_provider().cloned();
            let targets = self.resolve_distill_targets();
            tracing::info!(
                tier = ?targets.first().map(|t| t.tier),
                provider_id = %targets.first().map(|t| t.provider_id.clone()).unwrap_or_default(),
                model_id = %targets.first().map(|t| t.model_id.clone()).unwrap_or_default(),
                fallback_targets = targets.len().saturating_sub(1),
                "Distillation target resolved (with call-phase fallback chain)"
            );

            // Track which target produced the successful result so the post-
            // success bookkeeping uses the right `compact_model` (which may
            // differ from the top-priority one if a lower tier won).
            let mut succeeded: Option<(
                ResolvedDistill,
                String,
                (String, acowork_core::providers::traits::UsageInfo),
            )> = None;
            let mut last_err: Option<crate::error::RuntimeError> = None;
            for target in &targets {
                let (compact_provider, compact_model, _tier) = self.distill_provider(target);
                match self
                    .session
                    .history
                    .compact_via_llm(
                        compact_provider.as_ref(),
                        &compact_model,
                        system_prompt,
                        self.session.identity_context(),
                    )
                    .await
                {
                    Ok((summary, usage)) => {
                        tracing::info!(
                            target_provider = %target.provider_id,
                            target_model = %target.model_id,
                            tier = ?target.tier,
                            summary_len = summary.len(),
                            "Compaction LLM call succeeded"
                        );
                        succeeded = Some((target.clone(), compact_model, (summary, usage)));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target_provider = %target.provider_id,
                            target_model = %target.model_id,
                            tier = ?target.tier,
                            error = %e,
                            "Compaction LLM call failed, trying next tier"
                        );
                        last_err = Some(e);
                    }
                }
            }

            match succeeded {
                Some((_resolved_distill, compact_model, (summary, usage))) => {
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
                    crate::episode_distill::EpisodeDistiller::write_summary_to_provider(
                        &summary,
                        &session_id,
                        &memory_provider,
                        self.core.embedding_provider.as_deref(),
                    )
                    .await;

                    // Mark session as compacted (zero new messages since compaction)
                    self.session.is_compacted = true;

                    // ADR-051 P3: Run all post-compaction maintenance tasks
                    // (generalization + history compression + relationship)
                    // via a single MemoryManager high-level method.
                    self.run_post_compaction_memory_tasks().await;

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
                None => {
                    tracing::warn!(
                        error = %last_err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                        target_count = targets.len(),
                        "All distillation targets failed, falling back to FIFO + emergency trim"
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
            // ADR-060 §5.5: Block D — the current user message staged by
            // `run_inner`. `None` on the debug-replay path and for direct
            // loop usages without a staged message.
            self.pending_user_message.as_ref(),
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

        // LLM-perspective tool dump: AFTER builtin + MCP merge.
        // This is the most authoritative view of what the LLM actually sees
        // in the next provider call. tool_names is the literal contents of
        // the tools array sent on the wire -- filter it for any platform
        // tool like context_abandon / context_retrieve to verify hot reload
        // of the compression switch (ADR-052).
        if let Some(ref tools) = chat_request.tools {
            let tool_names: Vec<String> = tools
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            t.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                })
                .collect();
            tracing::info!(
                request_tools_count = tool_names.len(),
                request_tool_names = ?tool_names,
                "LLM tools schema (after builtin + MCP merge) -- exact wire payload"
            );
        }

        // Compute total input chars for next round's token ratio calibration.
        self.last_input_chars = count_chat_request_chars(&chat_request);

        // Inject transient tool results from the previous iteration
        // (ADR-032 C3a — preserved for future tools that need one-shot
        // injection). ADR-052 removed the `context_retrieve` transient
        // path; the field is now only populated by hypothetical future
        // transient tools. Empty vec naturally skips.
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

            // Find a UTF-8 safe cut point within budget.
            // Replaced ad-hoc loop with the project-wide `util::text::truncate_utf8`
            // so the same character-boundary guarantee lives in exactly one place.
            let safe = crate::util::text::truncate_utf8(result, max_chars.min(result.len()));
            let mut cut = safe.len();

            // Try to cut at a newline for readability
            if let Some(nl_pos) = safe.rfind('\n').filter(|&p| p > cut / 2) {
                cut = nl_pos;
            }

            let kept = &safe[..cut];
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

// ── Tests ─────────────────────────────────────────────────────────────────
//
// ADR-056 §9.1: unit tests for `resolve_distill_model` three-tier fallback.
//
// We exercise the resolver end-to-end by constructing a real `AgentLoop`
// (per the test scaffolding already in `loop_.rs::tests`) and then
// mutating its `AgentCore` fields (`global_provider_list`,
// `provider_key_vault`, `provider_compact_models`, `default_compact_model`)
// to stage each scenario. The resolver is purely a synchronous,
// self-contained function on `AgentLoop`, so this approach covers every
// branch without mocking anything.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_core::BuiltinToolEntry;
    use crate::agent::context::ContextBuilder;
    use crate::config::RuntimeConfig;
    use acowork_core::protocol::{
        ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry,
    };
    use acowork_core::providers::mock::{MockProvider, MockResponse};
    use acowork_core::providers::traits::{
        ChatMessage, ChatRequest, ChatResponse, MessageRole, StreamEvent, UsageInfo,
    };
    use acowork_core::{AgentManifest, Budget};
    use std::sync::Arc;

    // ── Fixtures ────────────────────────────────────────────────────────

    fn minimal_manifest() -> AgentManifest {
        AgentManifest::from_toml(
            r#"
            agent_id = "com.test.compact"
            version = "1.0.0"
            name = "Compact Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "deepseek"
            model = "deepseek-v4-flash"
            "#,
        )
        .expect("manifest must parse")
    }

    fn minimal_config() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    fn caps(context_window: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    fn build_loop() -> AgentLoop {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<BuiltinToolEntry> = vec![];
        let budget = Budget {
            daily_tokens: Some(100_000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        };
        let (loop_, _inbound_tx) = AgentLoop::new(
            minimal_config(),
            minimal_manifest(),
            provider,
            tools,
            budget,
            None,
            None,
        );
        loop_
    }

    /// Stage two providers in the AgentCore's global_provider_list:
    ///   `deepseek`     → deepseek-v4-flash (large), deepseek-v4-pro (large)
    ///   `ollama-local` → qwen2.5:0.5b (32K), llama3:8b (8K)
    ///
    /// Only `deepseek` gets an API key — `ollama-local` is a local
    /// provider with an empty key, mirroring the real-world setup where
    /// Ollama needs no key. `is_default_compact_provider_available` must
    /// accept it via the local base_url branch (ADR-056 §2.3).
    fn seed_providers(loop_: &AgentLoop) {
        let mut list = loop_.core.global_provider_list.write().unwrap();
        *list = vec![
            ProviderListItem {
                id: "deepseek".to_string(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                compact_model: Some("deepseek-v4-flash".to_string()),
                custom: false,
                models: vec![
                    ProviderModelEntry {
                        id: "deepseek-v4-flash".to_string(),
                        capabilities: caps(128_000),
                        max_output_tokens_limit: 32_768,
                    },
                    ProviderModelEntry {
                        id: "deepseek-v4-pro".to_string(),
                        capabilities: caps(128_000),
                        max_output_tokens_limit: 32_768,
                    },
                ],
            },
            ProviderListItem {
                id: "ollama-local".to_string(),
                base_url: "http://localhost:11434/v1".to_string(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                compact_model: None,
                custom: false,
                models: vec![
                    ProviderModelEntry {
                        id: "qwen2.5:0.5b".to_string(),
                        capabilities: caps(32_000),
                        max_output_tokens_limit: 16_384,
                    },
                    ProviderModelEntry {
                        id: "llama3:8b".to_string(),
                        capabilities: caps(8_000),
                        max_output_tokens_limit: 8_192,
                    },
                ],
            },
        ];
        drop(list);

        // Only the cloud provider (deepseek) gets a key; ollama-local stays
        // keyless (local provider, no key required).
        let mut keys = loop_.core.provider_key_vault.write().unwrap();
        keys.insert("deepseek".to_string(), "sk-test-deepseek".to_string());
    }

    fn set_session(loop_: &mut AgentLoop, provider: &str, model: &str) {
        loop_.session.provider = Some(provider.to_string());
        loop_.session.model = Some(model.to_string());
    }

    fn set_provider_compact(
        loop_: &mut AgentLoop,
        provider: &str,
        compact: Option<&str>,
    ) {
        loop_
            .core
            .provider_compact_models
            .insert(provider.to_string(), compact.map(|s| s.to_string()));
    }

    // ── resolve_distill_model: three-tier fallback ─────────────────────

    #[test]
    fn build_chat_request_block_layout_b_contains_current_and_d_duplicate() {
        // ADR-060 §5.5/§7.3: Block B is the full append-only history — it
        // INCLUDES the current user message — and Block D is a byte-identical
        // duplicate staged by `run_inner`. Request order: A → B → C → D.
        let mut loop_ = build_loop();
        loop_.session.history.append(ChatMessage::user("First turn"));
        loop_.session.history.append(ChatMessage::assistant("First reply"));
        let current = ChatMessage::user("Second turn");
        loop_.session.history.append(current.clone());
        loop_.pending_user_message = Some(current.clone());
        // Block C source: session todo snapshot.
        loop_.session.update_todos(
            vec![crate::agent::session_state::TodoItem {
                id: "t1".to_string(),
                content: "Task 1".to_string(),
                status: crate::agent::session_state::TodoStatus::Pending,
            }],
            false,
        );

        let mut builder = ContextBuilder::new("Kernel".to_string());
        let request = loop_.build_chat_request(&mut builder, "test-model");

        // [0] Block A: system kernel.
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert!(
            !request.messages[0].content.contains("Active Task List"),
            "Block A must not contain the dynamic todo snapshot"
        );

        // Block B: history turns present, including the current user message.
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.content == "Second turn"),
            "Block B must include the current user message"
        );

        // Block C: User role (never System), todo snapshot, after Block B.
        let block_c = request
            .messages
            .iter()
            .find(|m| m.content.contains("Active Task List"))
            .unwrap();
        assert_eq!(
            block_c.role, MessageRole::User,
            "Block C must use User role (ADR-060 §5.4)"
        );

        // Block D (last): byte-identical duplicate of the staged message.
        let block_d = request.messages.last().unwrap();
        assert_eq!(block_d.role, MessageRole::User);
        assert_eq!(block_d.content, current.content);
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.content == "Second turn")
                .count(),
            2,
            "current user message appears exactly twice: Block B copy + Block D duplicate"
        );
    }

    #[test]
    fn build_chat_request_block_d_none_in_tool_iteration() {
        use acowork_core::providers::traits::{FunctionCall, ToolCall};

        // ADR-060 §5.5/§7.3: tool-loop iterations have no staged message
        // (`run_inner` clears it) — the request ends with the tool result
        // and the original user turn appears only once (Block B).
        let mut loop_ = build_loop();
        loop_.session.history.append(ChatMessage::user("Turn"));
        // A REAL tool_call on the assistant turn keeps the tool result
        // from being classified as orphaned by sanitize_messages.
        loop_.session.history.append(ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "toolu_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "test_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        loop_.session.history.append(ChatMessage::tool("toolu_1", "ok"));
        loop_.pending_user_message = None;

        let mut builder = ContextBuilder::new("Kernel".to_string());
        let request = loop_.build_chat_request(&mut builder, "test-model");

        let last = request.messages.last().unwrap();
        assert_eq!(
            last.role,
            MessageRole::Tool,
            "tool iteration: last message must be the tool result, no Block D"
        );
        assert_eq!(
            request.messages.iter().filter(|m| m.content == "Turn").count(),
            1,
            "no Block D means the user turn appears exactly once"
        );
    }

    #[tokio::test]
    async fn auto_inject_does_not_set_flag_without_provider() {
        // ADR-060 §6.3: `memory_retrieved_for_session` is set only on a
        // SUCCESSFUL first retrieval. With no memory provider the call
        // bails early and the flag must stay false — a transient provider
        // absence must not permanently starve the session.
        let mut loop_ = build_loop();
        assert!(loop_.core.memory_provider().is_none());
        let mut builder = ContextBuilder::new("Kernel".to_string());
        loop_
            .retrieve_and_inject_memories("hello", &mut builder)
            .await;
        assert!(
            !loop_.memory_retrieved_for_session,
            "no provider → no retrieval → flag stays false"
        );
        // Repeated calls keep retrying (flag still false).
        loop_
            .retrieve_and_inject_memories("hello again", &mut builder)
            .await;
        assert!(!loop_.memory_retrieved_for_session);
    }

    #[test]
    fn tier1_global_default_is_picked_when_set_and_available() {
        // ADR-056 §1, §3.2: Global default wins when present and valid.
        // Real-world flagship scenario: local Ollama model selected as the
        // cross-provider default while the session chats with deepseek —
        // ollama-local has NO API key here and must still be accepted via
        // the local base_url branch.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));

        let resolved = loop_.resolve_distill_model("hello world");
        assert_eq!(resolved.provider_id, "ollama-local");
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    #[test]
    fn tier1_global_default_falls_through_when_provider_unavailable() {
        // ADR-056 §3.2 / §7: If the global provider has no API key,
        // fall back to Level 2 (provider.compact_model).
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        loop_.core.default_compact_model =
            Some(("anthropic".to_string(), "claude-3-haiku".to_string())); // not seeded

        let resolved = loop_.resolve_distill_model("hello world");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_global_default_falls_through_when_context_too_small() {
        // ADR-056 §3.2 / §7: Global model exists but context_window < estimate.
        // The estimated token count from `count_text("hello world")` is tiny,
        // but we force the scenario by using a model with a tiny context_window
        // that is below the estimate when the input is large enough.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        // Pick the tiny 8K model as global default; the probe_model-derived
        // estimate for a long input will exceed 8K → fall back.
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "llama3:8b".to_string()));

        // 50 KB of text → ~13k+ tokens by char/4 estimate, well above 8K.
        let long_input = "x".repeat(50_000);
        let resolved = loop_.resolve_distill_model(&long_input);
        // Tier 1 fails (context too small) → Tier 2 fires.
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_falls_through_when_model_not_in_capabilities() {
        // ADR-056 §7: If global default points at a model_id that doesn't
        // exist in any provider's models[], warn and fall back to Tier 2.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "ghost-model-99".to_string()));

        let resolved = loop_.resolve_distill_model("hi");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
    }

    #[test]
    fn no_global_default_goes_straight_to_tier2_provider_compact() {
        // ADR-056 §3.2 base case: global default is None -> Tier 2 fires.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        assert!(loop_.core.default_compact_model.is_none());

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_and_tier2_both_fail_goes_to_tier3_current_chat() {
        // Both global default and provider.compact_model are unusable.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        // Provider compact missing entirely.
        set_provider_compact(&mut loop_, "deepseek", None);

        // Global default -> unknown provider, will fall through.
        loop_.core.default_compact_model =
            Some(("anthropic".to_string(), "claude-3-haiku".to_string()));

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-pro");
        assert!(matches!(resolved.tier, DistillTier::CurrentChat));
    }

    #[test]
    fn tier3_falls_back_to_chat_model_when_session_provider_empty() {
        // Edge case: no session provider at all -> Tier 3 still resolves
        // to the current chat model with empty provider_id.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        loop_.session.provider = None;
        loop_.session.model = Some("deepseek-v4-pro".to_string());
        set_provider_compact(&mut loop_, "deepseek", None);
        loop_.core.default_compact_model = None;

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "");
        assert_eq!(resolved.model_id, "deepseek-v4-pro");
        assert!(matches!(resolved.tier, DistillTier::CurrentChat));
    }

    // ── helpers: is_default_compact_provider_available ────────────────

    #[test]
    fn default_compact_provider_available_local_without_key() {
        // ADR-056 §2.3: local provider (Ollama) needs no API key — the
        // availability check must accept it via the local base_url branch,
        // otherwise the flagship "chat=deepseek, distill=ollama" scenario
        // would never fire.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));
        assert!(
            loop_.core.is_default_compact_provider_available(),
            "local provider without key must be usable"
        );
    }

    #[test]
    fn default_compact_provider_unavailable_cloud_without_key() {
        // ADR-056 §7: cloud provider whose key was revoked is NOT callable
        // → distillation falls back to Level 2.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        loop_.core.default_compact_model =
            Some(("deepseek".to_string(), "deepseek-v4-flash".to_string()));
        assert!(loop_.core.is_default_compact_provider_available());

        // Revoke the key -> no longer available.
        loop_.core.provider_key_vault.write().unwrap().remove("deepseek");
        assert!(!loop_.core.is_default_compact_provider_available());
    }

    #[test]
    fn default_compact_provider_unavailable_when_not_set() {
        let loop_ = build_loop();
        assert!(!loop_.core.is_default_compact_provider_available());
    }

    // ── Token estimation uses compact model, not chat model ──────────
    //
    // Regression check for the bug fix in ADR-056 §5.2: `count_text` must
    // be called with the probe model derived from the three-tier chain
    // (compact model preferred), not the chat model. We can't observe
    // count_text directly, but we can prove the resolver picks the
    // compact model when one is available -- which means the chat model's
    // own context_window never demotes the resolver.

    #[test]
    fn resolved_distill_uses_compact_model_when_global_default_available() {
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        // Current chat model has small-ish context.
        set_session(&mut loop_, "deepseek", "deepseek-v4-flash");
        set_provider_compact(&mut loop_, "deepseek", None); // no Tier 2

        // Global default has ample context.
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));

        let resolved = loop_.resolve_distill_model("some long text");
        assert_eq!(resolved.provider_id, "ollama-local");
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    // ── resolve_distill_targets: call-phase fallback chain ────────────

    #[test]
    fn targets_list_returns_three_tiers_in_priority_order() {
        // All three tiers are reachable → list = [GlobalDefault,
        // ProviderCompact, CurrentChat].
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));

        let targets = loop_.resolve_distill_targets();
        assert_eq!(targets.len(), 3, "expected three targets, got {targets:?}");
        assert!(matches!(targets[0].tier, DistillTier::GlobalDefault));
        assert!(matches!(targets[1].tier, DistillTier::ProviderCompact));
        assert!(matches!(targets[2].tier, DistillTier::CurrentChat));
        assert_eq!(targets[0].provider_id, "ollama-local");
        assert_eq!(targets[1].provider_id, "deepseek");
        assert_eq!(targets[1].model_id, "deepseek-v4-flash");
        assert_eq!(targets[2].model_id, "deepseek-v4-pro");
    }

    #[test]
    fn targets_list_collapses_to_current_chat_when_only_tier3_is_available() {
        // No global default + no provider compact → only Level 3 is reachable.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", None);
        assert!(loop_.core.default_compact_model.is_none());

        let targets = loop_.resolve_distill_targets();
        assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
        assert!(matches!(targets[0].tier, DistillTier::CurrentChat));
        assert_eq!(targets[0].model_id, "deepseek-v4-pro");
    }

    #[test]
    fn targets_list_dedups_duplicate_provider_model_pairs() {
        // Edge case: all three tiers resolve to the same (provider, model)
        // (session pid = deepseek, chat = deepseek-v4-flash, provider compact
        // = deepseek-v4-flash, global default = deepseek-v4-flash). De-dup
        // must collapse the list to a single entry so the caller does not
        // retry the same broken target three times.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-flash");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        loop_.core.default_compact_model =
            Some(("deepseek".to_string(), "deepseek-v4-flash".to_string()));

        let targets = loop_.resolve_distill_targets();
        let unique: std::collections::HashSet<(String, String)> = targets
            .iter()
            .map(|t| (t.provider_id.clone(), t.model_id.clone()))
            .collect();
        assert_eq!(
            unique.len(),
            targets.len(),
            "duplicate (provider, model) pairs not allowed"
        );
        assert!(
            targets.len() <= 2,
            "expected at most 2 distinct targets (global default + maybe one other), got {targets:?}"
        );
        assert!(targets.iter().any(|t| matches!(t.tier, DistillTier::GlobalDefault)));
    }

    // ── compact_history_if_needed: call-phase fallback integration ────

    /// Wrap a MockProvider so the FIRST call to chat() returns Error and
    /// subsequent calls return Text. The shared state (`Arc<Mutex>`) lets
    /// the integration test inspect the model names of every call.
    struct Tier1FailThenSucceedProvider {
        responses: std::sync::Arc<std::sync::Mutex<Vec<MockResponse>>>,
        call_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Tier1FailThenSucceedProvider {
        fn new(fail_count: usize, success_text: &str) -> Self {
            let mut responses = Vec::new();
            for i in 0..fail_count {
                responses.push(MockResponse::Error {
                    message: format!("provider failed at call {i}"),
                });
            }
            responses.push(MockResponse::Text {
                content: success_text.to_string(),
            });
            Self {
                responses: std::sync::Arc::new(std::sync::Mutex::new(responses)),
                call_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
        fn log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
            self.call_log.clone()
        }
    }

    #[async_trait::async_trait]
    impl Provider for Tier1FailThenSucceedProvider {
        fn name(&self) -> &str {
            "tier1-fail-then-succeed"
        }
        async fn chat(
            &self,
            request: ChatRequest,
        ) -> acowork_core::error::Result<ChatResponse> {
            self.call_log
                .lock()
                .unwrap()
                .push(request.model.clone());
            let mut resp = self.responses.lock().unwrap();
            let next = if resp.is_empty() {
                MockResponse::Text {
                    content: "default".to_string(),
                }
            } else {
                resp.remove(0)
            };
            match next {
                MockResponse::Text { content } => Ok(ChatResponse {
                    content,
                    usage: Some(UsageInfo {
                        prompt_tokens: 50,
                        completion_tokens: 25,
                        total_tokens: 75,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                MockResponse::Error { message } => Err(
                    acowork_core::AcoworkError::Provider(
                        acowork_core::providers::ProviderError::unknown(message),
                    ),
                ),
                _ => unimplemented!(),
            }
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<
            Box<dyn futures_core::Stream<Item = StreamEvent> + Send>,
        > {
            unimplemented!()
        }
        async fn chat_token_count(
            &self,
            messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(messages.iter().map(|m| m.content.len() as u64 / 4).sum())
        }
    }

    /// Provider that ALWAYS errors (for the "all tiers fail" test).
    struct AlwaysFailProvider;

    #[async_trait::async_trait]
    impl Provider for AlwaysFailProvider {
        fn name(&self) -> &str {
            "always-fail"
        }
        async fn chat(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<ChatResponse> {
            Err(acowork_core::AcoworkError::Provider(
                acowork_core::providers::ProviderError::unknown("permanent failure".to_string()),
            ))
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<
            Box<dyn futures_core::Stream<Item = StreamEvent> + Send>,
        > {
            unimplemented!()
        }
        async fn chat_token_count(
            &self,
            _messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn call_phase_fallback_skips_failing_tier_and_uses_next() {
        // Scenario: GlobalDefault = ollama-local (unreachable in tests),
        // so the resolver falls back to Tier 2 (ProviderCompact =
        // deepseek-v4-flash) then Tier 3 (CurrentChat = deepseek-v4-pro).
        // We install a session provider mock that fails on the FIRST call
        // (= Tier 2: deepseek-v4-flash) and succeeds thereafter (= Tier 3:
        // deepseek-v4-pro). The integration test asserts:
        //   - Tier 2 was attempted and failed
        //   - Tier 3 was attempted next and succeeded
        //   - History was compacted (compaction marker present)
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        // GlobalDefault points to ollama-local which is NOT seeded as a
        // provider with a key, so `resolve_distill_model` skips it at
        // selection time and starts at Tier 2.
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));

        let _targets = loop_.resolve_distill_targets();
        // The session_pid is deepseek; deepseek has an API key, so the
        // selection-time fallback should NOT exclude Tier 1 here.
        // To force the *call-phase* fallback, install a provider mock
        // that fails on the first call regardless of which tier it is.
        // Three targets → three attempts. First call fails, second succeeds.
        let fail_once = Tier1FailThenSucceedProvider::new(1, "<summary>ok</summary>");
        let call_log = fail_once.log();
        loop_.core.update_provider(
            std::sync::Arc::new(fail_once) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );

        // Build a non-trivial history so compaction has something to do.
        for i in 0..10 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Message #{i}"),
                ..Default::default()
            });
            loop_.session.history.append(ChatMessage {
                role: MessageRole::Assistant,
                content: format!("Reply #{i}"),
                ..Default::default()
            });
        }

        loop_.compact_history_if_needed("deepseek-v4-pro", true).await;

        let log = call_log.lock().unwrap();
        assert!(
            log.len() >= 2,
            "expected fallback to retry; actual call log: {log:?}"
        );

        // History was actually compacted (replace_middle_with_summary ran)
        let has_marker = loop_
            .session
            .history
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME));
        assert!(
            has_marker,
            "compaction marker should be present after fallback succeeded"
        );
    }

    #[tokio::test]
    async fn call_phase_fallback_when_all_tiers_fail_does_fifo_trim() {
        // When ALL tiers fail (provider mock errors forever), history must
        // fall back to trim_fifo + emergency_trim — the documented
        // last-resort behaviour. The user loses early context, but the
        // session can still continue (better than a panic or hanging
        // request).
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        loop_.core.default_compact_model =
            Some(("ollama-local".to_string(), "qwen2.5:0.5b".to_string()));

        loop_.core.update_provider(
            std::sync::Arc::new(AlwaysFailProvider) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );

        for i in 0..20 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Long message {i} ").repeat(50),
                ..Default::default()
            });
        }

        let tokens_before = loop_.session.history.token_count();
        loop_.compact_history_if_needed("deepseek-v4-pro", true).await;
        let tokens_after = loop_.session.history.token_count();

        // No compaction marker — the trim path was taken instead.
        let has_marker = loop_
            .session
            .history
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME));
        assert!(
            !has_marker,
            "no compaction marker should exist when LLM compaction fully failed"
        );

        // History must not grow (FIFO+emergency path is the floor).
        assert!(
            tokens_after <= tokens_before,
            "FIFO path must not grow history: before={tokens_before} after={tokens_after}"
        );
    }
}
