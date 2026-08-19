//! Debug Protocol business logic handlers (ADR-048).
//!
//! Each RPC method's business code lives here as an independent
//! `pub async fn`, transport-free and reusable from any frontend
//! (HTTP REST via `http/debug.rs`).
//!
//! All functions take a mutable `&mut DebugController` so they share the
//! same locking discipline across transports. They return [`DebugError`]
//! so the caller (`DebugService` in `usecases/debug_service_impl.rs`)
//! can map transport-specific errors uniformly.
//!
//! **Business logic unchanged**: the bodies were copied verbatim from the
//! pre-ADR-048 WebSocket route closure. Only the function signature
//! changed: the lock is acquired by the caller, so each handler
//! operates on `&mut DebugController` instead of going through `&mut self`.

use std::sync::Arc;

use serde::Serialize;

use super::controller::{DebugController, DebugState};
use super::events::DebugEventSender;
use super::protocol::{
    ContextSections, DebugPhase, DebugUsage, GetContextSnapshotParams, GetContextSnapshotResult,
    GetSectionParams, GetSectionResult, PatchContextParams, RewindParams, RewindResult,
    StepGranularity,
};

/// Error type returned by every debug handler.
///
/// Maps naturally to HTTP status codes - the conversion is the
/// caller's responsibility (see `http/debug.rs`).
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    /// Caller requested an unknown session ID. Maps to HTTP `404`.
    #[error("No debug session found for session_id: {0}")]
    SessionNotFound(String),
    /// Method parameters failed validation. Maps to HTTP `400`.
    #[error("Invalid params: {0}")]
    InvalidParams(String),
    /// Snapshot / section lookup failed. Maps to HTTP `404`.
    #[error("Not found: {0}")]
    NotFound(String),
    /// Controller's internal state forbids the operation (e.g. step on
    /// non-paused). Maps to HTTP `409`.
    #[error("Invalid state: {0}")]
    InvalidState(String),
    /// Unspecified internal failure. Maps to HTTP `422`.
    #[error("Internal error: {0}")]
    Internal(String),
}

impl DebugError {
    /// Stable numeric error code for this error. The numbering was
    /// inherited from the pre-ADR-048 JSON-RPC codes; the HTTP error
    /// body (`http/debug.rs`) carries it so the Desktop can branch on
    /// it without parsing messages.
    pub fn rpc_code(&self) -> i32 {
        match self {
            DebugError::SessionNotFound(_) => -32000,
            DebugError::InvalidParams(_) => -32602,
            DebugError::NotFound(_) => -32002,
            DebugError::InvalidState(_) => -32003,
            DebugError::Internal(_) => -32603,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Send a tagged event through the per-session `DebugEventSender`,
/// tagging it with the originating session ID.
///
/// This is a tiny wrapper to keep call sites readable. The event is sent
/// on the unbounded channel (non-blocking); if the channel is closed
/// (e.g. server was shut down), the event is silently dropped — debug
/// events are best-effort.
fn send_event(event_tx: &DebugEventSender, event: super::events::DebugEvent) {
    event_tx.send(event);
}

// ── Execution Control (5 methods) ─────────────────────────────────────

/// `debugger.resume` — resume auto-execution.
///
/// Transition: any state → Running. Notifies the SessionTask via the
/// resume_notify handle so it wakes up if it was blocked waiting for
/// input. Emits an ExecutionStateChanged event.
pub async fn handle_resume(
    ctrl: &mut DebugController,
    event_tx: &DebugEventSender,
) -> Result<(), DebugError> {
    ctrl.state = DebugState::Running;
    let iteration = ctrl.iteration;
    send_event(
        event_tx,
        super::events::DebugEvent::ExecutionStateChanged {
            new_state: DebugState::Running,
            iteration,
        },
    );
    tracing::info!("Debug: resume — agent loop will continue");
    Ok(())
}

/// `debugger.pause` — pause auto-execution.
///
/// Transition: any state → Paused. Fires control_notify so any blocked
/// `select!` branch in the agent loop wakes immediately.
pub async fn handle_pause(
    ctrl: &mut DebugController,
    event_tx: &DebugEventSender,
) -> Result<(), DebugError> {
    ctrl.state = DebugState::Paused;
    let iteration = ctrl.iteration;
    ctrl.notify_control();
    send_event(
        event_tx,
        super::events::DebugEvent::ExecutionStateChanged {
            new_state: DebugState::Paused,
            iteration,
        },
    );
    tracing::info!("Debug: pause — agent loop will pause immediately");
    Ok(())
}

/// `debugger.step` — execute one step then auto-pause.
///
/// Returns `InvalidState` if the controller is not currently Paused.
/// Emits an ExecutionStateChanged event on transition.
pub async fn handle_step(
    ctrl: &mut DebugController,
    event_tx: &DebugEventSender,
    _granularity: StepGranularity,
) -> Result<StepOutcome, DebugError> {
    let iteration = ctrl.iteration;
    if ctrl.state != DebugState::Paused {
        tracing::info!(state = ?ctrl.state, "Debug: step ignored — not paused");
        return Ok(StepOutcome::Ignored {
            state: ctrl.state,
            iteration,
        });
    }
    ctrl.state = DebugState::Stepping;
    send_event(
        event_tx,
        super::events::DebugEvent::ExecutionStateChanged {
            new_state: DebugState::Stepping,
            iteration,
        },
    );
    tracing::info!("Debug: step — agent loop will execute one step");
    Ok(StepOutcome::Accepted)
}

/// Outcome of a `step` request — kept as a typed sum so callers can
/// distinguish "step queued" vs "ignored because not paused" without
/// parsing strings.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StepOutcome {
    Accepted,
    Ignored { state: DebugState, iteration: u32 },
}

/// `debugger.stop` — terminate the agent loop.
///
/// Transition: any state → Stopped. Fires control_notify so any blocked
/// `select!` branch wakes immediately and the loop sees the state.
pub async fn handle_stop(
    ctrl: &mut DebugController,
    event_tx: &DebugEventSender,
) -> Result<(), DebugError> {
    ctrl.state = DebugState::Stopped;
    let iteration = ctrl.iteration;
    ctrl.notify_control();
    send_event(
        event_tx,
        super::events::DebugEvent::ExecutionStateChanged {
            new_state: DebugState::Stopped,
            iteration,
        },
    );
    tracing::info!("Debug: stop — agent loop terminated");
    Ok(())
}

// ── State Query (3 methods) ───────────────────────────────────────────

/// Full debug state snapshot — returned by `debugger.getState`.
#[derive(Debug, Clone, Serialize)]
pub struct DebugStateSnapshot {
    pub iteration: u32,
    pub phase: DebugPhase,
    pub messages: Vec<serde_json::Value>,
    pub snapshot_ids: Vec<String>,
    pub usage: DebugUsage,
    pub paused: bool,
    pub state: String,
}

/// `debugger.getState` — full controller state.
pub async fn handle_get_state(
    ctrl: &mut DebugController,
) -> Result<DebugStateSnapshot, DebugError> {
    let current_state = ctrl.state;
    let state = DebugStateSnapshot {
        iteration: ctrl.iteration,
        phase: ctrl.phase,
        messages: Vec::new(), // TODO: populate in S2.3 with actual messages
        snapshot_ids: ctrl
            .conversation_snapshots
            .iter()
            .map(|s| s.id.clone())
            .collect(),
        usage: DebugUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        paused: current_state == DebugState::Paused,
        state: serde_json::to_string(&current_state)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string(),
    };
    tracing::debug!(
        iteration = ctrl.iteration,
        dbg_state = %state.state,
        "Debug: getState response"
    );
    Ok(state)
}

// ── Context Snapshots (2 methods) ─────────────────────────────────────

/// `debugger.getContextSnapshot` — full snapshot metadata for one iteration.
pub async fn handle_get_context_snapshot(
    ctrl: &mut DebugController,
    params: GetContextSnapshotParams,
) -> Result<GetContextSnapshotResult, DebugError> {
    match ctrl.get_context_snapshot(params.iteration) {
        Some(snap) => {
            let sections = ContextSections::from(&snap.sections);
            Ok(GetContextSnapshotResult {
                iteration: snap.iteration,
                built_at: snap.built_at.to_rfc3339(),
                sections,
                total_token_estimate: snap.total_token_estimate,
                phase: DebugPhase::BuildContext,
            })
        }
        None => Err(DebugError::NotFound(format!(
            "No context snapshot for iteration {}",
            params.iteration
        ))),
    }
}

/// `debugger.getSection` — full content for one section of one iteration.
pub async fn handle_get_section(
    ctrl: &mut DebugController,
    params: GetSectionParams,
) -> Result<GetSectionResult, DebugError> {
    tracing::info!(
        iteration = params.iteration,
        section = %params.section,
        "Debug: getSection request"
    );
    let snap = ctrl.get_context_snapshot(params.iteration).ok_or_else(|| {
        DebugError::NotFound(format!(
            "No context snapshot for iteration {}",
            params.iteration
        ))
    })?;
    let section_content = match params.section.as_str() {
        "system_prompt" => &snap.sections.system_prompt,
        "workspace_context" => &snap.sections.workspace_context,
        "environment" => &snap.sections.environment,
        "tool_definitions" => &snap.sections.tool_definitions,
        "skill_instructions" => &snap.sections.skill_instructions,
        "retrieved_memory" => &snap.sections.retrieved_memory,
        "identity_context" => &snap.sections.identity_context,
        _ => {
            return Err(DebugError::InvalidParams(format!(
                "Unknown section: {}",
                params.section
            )));
        }
    };
    tracing::info!(
        iteration = params.iteration,
        section = %params.section,
        content_len = section_content.content.len(),
        "Debug: getSection returning result"
    );
    Ok(GetSectionResult {
        content: section_content.content.clone(),
        hash: section_content.hash.clone(),
        token_count: section_content.token_estimate,
    })
}

// ── Context Editing (3 methods) ────────────────────────────────────────

/// `debugger.rewind` — rewind the conversation to a previous iteration.
///
/// When rewind is invoked while Stopped, transition back to Paused.
pub async fn handle_rewind(
    ctrl: &mut DebugController,
    event_tx: &DebugEventSender,
    params: RewindParams,
) -> Result<RewindResult, DebugError> {
    let target = params.to_iteration;

    // When rewind is invoked while Stopped, transition back to
    // Paused.  Rewind is an explicit user action signalling
    // intent to continue working from a previous iteration.
    // Without this transition, await_debug_resume() returns
    // false immediately and agent_loop.run() short-circuits
    // with "Agent stopped by debugger", making rewind
    // effectively useless after Stop.
    let was_stopped = ctrl.state == DebugState::Stopped;
    if was_stopped {
        ctrl.state = DebugState::Paused;
    }

    // Reset iteration counter immediately so that getState
    // and any other consumers see the correct value without
    // waiting for the SessionTask to consume rewind_target.
    ctrl.iteration = target;

    // Store rewind target for consumer to apply
    ctrl.rewind_target = Some(target);
    // Notify consumers (await_debug_resume + SessionTask)
    // that a rewind is pending, eliminating the need for
    // polling.
    ctrl.notify_rewind();
    // Clear any pending patches — rewind supersedes patches
    ctrl.pending_patches = None;
    // Truncate snapshots after the target iteration
    ctrl.truncate_snapshots_after(target);

    // Find the message_count from the matching snapshot
    let message_count = ctrl
        .conversation_snapshots
        .iter()
        .find(|s| s.iteration == target)
        .map(|s| s.message_count)
        .unwrap_or(0);

    tracing::info!(
        target_iteration = target,
        message_count,
        was_stopped,
        "Debug: rewind — history will be truncated, patches cleared"
    );

    // If state transitioned from Stopped → Paused, push
    // an ExecutionStateChanged event so the frontend's debug
    // panel updates to show "Paused" instead of "Stopped".
    if was_stopped {
        send_event(
            event_tx,
            super::events::DebugEvent::ExecutionStateChanged {
                new_state: DebugState::Paused,
                iteration: target,
            },
        );
    }

    Ok(RewindResult {
        rewound_to_iteration: target,
        messages_trimmed_to: message_count,
    })
}

/// `debugger.patchContext` — merge context patches into the snapshot.
pub async fn handle_patch_context(
    ctrl: &mut DebugController,
    params: PatchContextParams,
) -> Result<(), DebugError> {
    // Bug 2 fix: merge incrementally instead of replacing
    let merged_patches = match ctrl.pending_patches.take() {
        Some(existing) => {
            let mut merged = existing;
            merged.merge(params.patches);
            merged
        }
        None => params.patches,
    };

    // Bug 3 fix: reflect patches in the context snapshot so
    // getSection returns the patched content, not the original.
    // merged_patches is owned (not borrowed from ctrl), so no borrow conflict.
    let current_iter = ctrl.iteration;
    // Use the model stored by capture_context_snapshot for
    // model-aware token counting via the unified API.
    // Clone before get_mut() to avoid borrow conflict.
    let model_owned = ctrl.current_model.clone().unwrap_or_default();
    let model: &str = &model_owned;
    if let Some(snap) = ctrl.context_snapshots.get_mut(&current_iter) {
        if let Some(ref prompt) = merged_patches.system_prompt {
            snap.sections.system_prompt =
                super::controller::SectionContent::new(prompt.clone(), model);
        }
        if let Some(ref tools) = merged_patches.tool_definitions {
            let content = serde_json::to_string_pretty(tools)
                .unwrap_or_else(|_| serde_json::to_string(tools).unwrap_or_default());
            snap.sections.tool_definitions = super::controller::SectionContent::new(content, model);
        }
        if let Some(ref skills) = merged_patches.skill_instructions {
            snap.sections.skill_instructions =
                super::controller::SectionContent::new(skills.clone(), model);
        }
        if let Some(ref memory) = merged_patches.retrieved_memory {
            let content = memory.to_string();
            snap.sections.retrieved_memory = super::controller::SectionContent::new(content, model);
        }
        if let Some(ref identity) = merged_patches.identity_context {
            let content = identity.to_string();
            snap.sections.identity_context = super::controller::SectionContent::new(content, model);
        }
        if let Some(ref workspace) = merged_patches.workspace_context {
            snap.sections.workspace_context =
                super::controller::SectionContent::new(workspace.clone(), model);
        }
        if let Some(ref env) = merged_patches.environment {
            // Empty string clears the override — build() falls back
            // to auto-detect.  The snapshot must match this behavior.
            let content = if env.is_empty() {
                crate::agent::context::detect_environment_text()
            } else {
                env.clone()
            };
            snap.sections.environment = super::controller::SectionContent::new(content, model);
        }
        tracing::info!(
            iteration = current_iter,
            "Debug: context snapshot updated with patched content"
        );
    } else {
        tracing::warn!(
            iteration = current_iter,
            "Debug: patchContext — no context snapshot to update"
        );
    }

    ctrl.pending_patches = Some(merged_patches);

    tracing::info!("Debug: context patches merged and stored for next reExecute");
    Ok(())
}

/// `debugger.reExecute` — re-run the current iteration with pending patches.
pub async fn handle_re_execute(ctrl: &mut DebugController) -> Result<ReExecuteOutcome, DebugError> {
    // Set re-execute pending flag for SessionTask to consume
    ctrl.set_re_execute_pending();
    // Set state to Running so the agent loop can proceed
    ctrl.state = DebugState::Running;
    tracing::info!(
        "Debug: reExecute — pending flag set, execution will proceed with patches (if any)"
    );
    Ok(ReExecuteOutcome {
        has_patches: ctrl.pending_patches.is_some(),
    })
}

/// Outcome of `reExecute` — carries whether patches were applied.
#[derive(Debug, Clone, Serialize)]
pub struct ReExecuteOutcome {
    pub has_patches: bool,
}

/// Convenience alias for `&Arc<Notify>` shared with `DebugController`.
/// Returned by `notify_*_handle()` getters; used by callers that need to
/// `notify_one()` from outside the controller.
pub type NotifyHandle = Arc<tokio::sync::Notify>;
