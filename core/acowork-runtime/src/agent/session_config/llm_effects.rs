//! LLM-side effects of config changes (ADR-047 §3.3.2).
//!
//! Called by SessionTask at turn boundaries when the config version has
//! changed. This is the ONLY place that handles LLM-side reactions to
//! config changes.
//!
//! Design: compares the current snapshot with the previous one to
//! determine which fields changed, then applies only the necessary
//! LLM-side effects. This avoids unnecessary provider rebuilds when
//! only unrelated fields (e.g. title) changed.

use acowork_core::providers::traits::ReasoningEffort;

use crate::agent::context::ContextBuilder;
use crate::agent::loop_::AgentLoop;
use crate::agent::session_config::SessionConfigSnapshot;

/// Apply LLM-side effects of a config change.
///
/// Called by SessionTask at turn boundaries when config version has changed.
/// This is the ONLY place that handles LLM-side reactions to config changes.
///
/// # Arguments
/// * `agent_loop` - The per-session AgentLoop (mutable, owns SessionState + AgentCore)
/// * `context_builder` - The per-session ContextBuilder (mutable)
/// * `snapshot` - Current config snapshot (post-change)
/// * `prev` - Previous config snapshot (pre-change, for diffing)
pub fn apply_llm_effects(
    agent_loop: &mut AgentLoop,
    context_builder: &mut ContextBuilder,
    snapshot: &SessionConfigSnapshot,
    prev: &SessionConfigSnapshot,
) {
    let model_changed = snapshot.model != prev.model;
    let provider_changed = snapshot.provider != prev.provider;

    // ── Model/Provider change -> rebuild LLM Provider ──────────────
    if (model_changed || provider_changed)
        && let Some(ref model) = snapshot.model
    {
        tracing::info!(
            model = %model,
            provider = ?snapshot.provider,
            "apply_llm_effects: model/provider changed, rebuilding LLM provider"
        );

        // Update in-memory SessionState
        agent_loop.session.set_model(model.clone());
        if let Some(ref provider_id) = snapshot.provider {
            agent_loop.session.set_provider(provider_id.clone());

            // Rebuild the LLM Provider instance from the shared global cache.
            if let Some(new_provider) = agent_loop.session_core.build_provider_for(
                provider_id,
                &agent_loop.core.config,
                &agent_loop.core.global_provider_list,
                &agent_loop.core.provider_key_vault,
                agent_loop.core.compat_cache.as_ref(),
            ) {
                agent_loop.update_provider(
                    new_provider,
                    model.clone(),
                    Some(provider_id.clone()),
                );
            } else {
                tracing::warn!(
                    provider_id = %provider_id,
                    "apply_llm_effects: provider not found in global cache, keeping current Provider instance"
                );
            }
        }

        // Update context builder for next iteration
        context_builder.set_override_model(model.clone());

        // Model switch resets reasoning_effort to new model's default
        // (clears any user override). Three-level priority chain:
        // 1. provider capabilities default_reasoning_effort
        // 2. Auto (if supports_reasoning is true)
        // 3. None (provider does not support thinking control)
        let caps = agent_loop.core.get_model_capabilities(model);
        let provider_default = caps
            .as_ref()
            .and_then(|c| c.default_reasoning_effort.clone());
        let default_effort = provider_default
            .as_deref()
            .and_then(ReasoningEffort::from_str_loose)
            .or_else(|| {
                if caps.as_ref().and_then(|c| c.supports_reasoning).unwrap_or(false) {
                    Some(ReasoningEffort::Auto)
                } else {
                    None
                }
            });
        agent_loop.session.set_reasoning_effort(default_effort.clone());

        // Persist new default effort to ConversationSession so resume
        // is consistent. This does NOT go through apply_config() (which
        // would increment config_version and trigger another cycle);
        // it's a direct write of the LLM-side reset, not an external
        // config mutation.
        if let Some(conv) = agent_loop.session.conversation() {
            let effort_str = default_effort.as_ref().map(|e| e.to_string());
            conv.update_reasoning_effort(effort_str);
        }
    }

    // ── ReasoningEffort change (without model switch) ──────────────
    if !model_changed
        && snapshot.reasoning_effort != prev.reasoning_effort
        && let Some(ref effort) = snapshot.reasoning_effort
    {
        let parsed = ReasoningEffort::from_str_loose(effort);
        tracing::info!(
            effort = %effort,
            parsed = ?parsed,
            "apply_llm_effects: reasoning_effort changed (no model switch)"
        );
        agent_loop.session.set_reasoning_effort(parsed);
    }
}
