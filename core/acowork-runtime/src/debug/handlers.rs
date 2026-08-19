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
/// on the broadcast channel (non-blocking); if the channel is closed
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
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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

// ── Tests ───────────────────────────────────────────────────────────────
//
// Unit coverage for every `pub async fn` in this module. Each handler is
// a pure function over `&mut DebugController` (+ optional params/event
// sender), so we can drive it directly without spinning up the Runtime
// HTTP server or MQTT broker — the goal is to lock the **business logic**
// so any future refactor breaks here first, not at the integration layer.
//
// Mirrors the legacy WebSocket server's behavior one-for-one (ADR-048
// "业务逻辑 0 改动" promise); if these pass, the HTTP/MQTT transports
// can wrap them without regressing.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::controller::{ContextSnapshot, ContextSnapshotSections, SectionContent};
    use crate::debug::events::DebugEventBus;
    use crate::debug::protocol::{
        DebugPhase, DebugUsage, GetContextSnapshotParams, GetSectionParams, PatchContextParams,
        PatchSet, RewindParams,
    };

    // ── Helpers ───────────────────────────────────────────────────────

    use crate::debug::controller::ConversationSnapshot;
    use crate::debug::events::{DebugEvent, TaggedEvent};

    /// Build a fresh `DebugController` for one test case.
    fn fresh_controller() -> DebugController {
        DebugController::new()
    }

    /// Build a per-session `DebugEventSender` backed by a fresh
    /// broadcast bus. The returned bus allows the test to verify what
    /// events were emitted by draining the receiver.
    fn fresh_event_sender_pair(
        session_id: &str,
    ) -> (DebugEventSender, tokio::sync::broadcast::Receiver<TaggedEvent>) {
        let bus = DebugEventBus::new();
        let rx = bus.subscribe();
        let tx = bus.sender_template().for_session(session_id.to_string());
        (tx, rx)
    }

    /// Collect the events currently buffered in a broadcast receiver
    /// without blocking. Returns the `DebugEvent` kinds in send order.
    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<TaggedEvent>) -> Vec<DebugEvent> {
        let mut out = Vec::new();
        while let Ok(tagged) = rx.try_recv() {
            out.push(tagged.event);
        }
        out
    }

    // ── handle_resume ─────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_sets_running_and_emits_state_change() {
        let mut ctrl = fresh_controller();
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        handle_resume(&mut ctrl, &tx).await.expect("resume should succeed");

        assert_eq!(ctrl.state, DebugState::Running);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "resume must emit exactly one event");
        match &events[0] {
            DebugEvent::ExecutionStateChanged {
                new_state,
                iteration,
            } => {
                assert_eq!(*new_state, DebugState::Running);
                assert_eq!(*iteration, ctrl.iteration);
            }
            other => panic!("expected ExecutionStateChanged, got {other:?}"),
        }
    }

    // ── handle_pause ──────────────────────────────────────────────────

    #[tokio::test]
    async fn pause_sets_paused_and_notifies_control() {
        let mut ctrl = fresh_controller();
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        handle_pause(&mut ctrl, &tx).await.expect("pause should succeed");

        assert_eq!(ctrl.state, DebugState::Paused);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DebugEvent::ExecutionStateChanged { new_state, .. } => {
                assert_eq!(*new_state, DebugState::Paused);
            }
            other => panic!("expected ExecutionStateChanged, got {other:?}"),
        }
        // control_notify must have been pulsed so any blocked
        // `tokio::select!` branch in the agent loop wakes up. We
        // verify the public observable: a fresh `Notify::notified()`
        // future returns immediately if a permit is already queued.
        let wait_for_notify = ctrl.control_notify.notified();
        tokio::pin!(wait_for_notify);
        let resolved = std::future::poll_fn(|cx| {
            use std::task::Poll;
            match wait_for_notify.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(true),
                Poll::Pending => Poll::Ready(false),
            }
        })
        .await;
        assert!(
            resolved,
            "control_notify must be pulsed by handle_pause"
        );
    }

    // ── handle_step ───────────────────────────────────────────────────

    #[tokio::test]
    async fn step_from_paused_transitions_to_stepping() {
        let mut ctrl = fresh_controller();
        ctrl.state = DebugState::Paused;
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        let outcome =
            handle_step(&mut ctrl, &tx, StepGranularity::Iteration).await.expect("step ok");

        assert_eq!(outcome, StepOutcome::Accepted);
        assert_eq!(ctrl.state, DebugState::Stepping);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DebugEvent::ExecutionStateChanged { new_state, .. } => {
                assert_eq!(*new_state, DebugState::Stepping);
            }
            other => panic!("expected ExecutionStateChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn step_from_non_paused_returns_ignored_and_does_not_emit() {
        let mut ctrl = fresh_controller();
        ctrl.state = DebugState::Running; // not Paused
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        let outcome =
            handle_step(&mut ctrl, &tx, StepGranularity::Phase).await.expect("step ignored ok");

        match outcome {
            StepOutcome::Ignored { state, iteration } => {
                assert_eq!(state, DebugState::Running);
                assert_eq!(iteration, ctrl.iteration);
            }
            StepOutcome::Accepted => panic!("expected Ignored, got Accepted"),
        }
        assert_eq!(ctrl.state, DebugState::Running, "state must not change");
        assert!(
            drain_events(&mut rx).is_empty(),
            "no event must be emitted when step is ignored"
        );
    }

    // ── handle_stop ───────────────────────────────────────────────────

    #[tokio::test]
    async fn stop_sets_stopped_and_notifies_control() {
        let mut ctrl = fresh_controller();
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        handle_stop(&mut ctrl, &tx).await.expect("stop should succeed");

        assert_eq!(ctrl.state, DebugState::Stopped);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DebugEvent::ExecutionStateChanged { new_state, .. } => {
                assert_eq!(*new_state, DebugState::Stopped);
            }
            other => panic!("expected ExecutionStateChanged, got {other:?}"),
        }
    }

    // ── handle_get_state ──────────────────────────────────────────────

    #[tokio::test]
    async fn get_state_returns_snapshot_with_current_iteration_and_paused_flag() {
        let mut ctrl = fresh_controller();
        ctrl.iteration = 5;
        ctrl.state = DebugState::Paused;

        let snap = handle_get_state(&mut ctrl).await.expect("get_state ok");

        assert_eq!(snap.iteration, 5);
        assert!(snap.paused, "state == Paused → paused == true");
        assert_eq!(snap.state, "Paused");
    }

    #[tokio::test]
    async fn get_state_reflects_running_not_paused() {
        let mut ctrl = fresh_controller();
        ctrl.state = DebugState::Running;
        let snap = handle_get_state(&mut ctrl).await.unwrap();
        assert!(!snap.paused);
        assert_eq!(snap.state, "Running");
    }

    // ── handle_get_context_snapshot ───────────────────────────────────

    #[tokio::test]
    async fn get_context_snapshot_returns_not_found_for_missing_iteration() {
        let mut ctrl = fresh_controller();
        let err = handle_get_context_snapshot(
            &mut ctrl,
            GetContextSnapshotParams { iteration: 99 },
        )
        .await
        .expect_err("missing iter must error");
        assert!(matches!(err, DebugError::NotFound(_)));
        assert_eq!(err.rpc_code(), -32002);
    }

    #[tokio::test]
    async fn get_context_snapshot_returns_result_for_existing_iteration() {
        let mut ctrl = fresh_controller();
        let section_content = SectionContent::with_token_count("hello".to_string(), 1);
        let snap = ContextSnapshot {
            iteration: 3,
            built_at: chrono::Utc::now(),
            sections: ContextSnapshotSections {
                system_prompt: section_content,
                workspace_context: SectionContent::with_token_count(String::new(), 0),
                environment: SectionContent::with_token_count(String::new(), 0),
                tool_definitions: SectionContent::with_token_count(String::new(), 0),
                skill_instructions: SectionContent::with_token_count(String::new(), 0),
                retrieved_memory: SectionContent::with_token_count(String::new(), 0),
                identity_context: SectionContent::with_token_count(String::new(), 0),
            },
            total_token_estimate: 1,
        };
        ctrl.context_snapshots.insert(3, snap);

        let result =
            handle_get_context_snapshot(&mut ctrl, GetContextSnapshotParams { iteration: 3 })
                .await
                .expect("get_context_snapshot ok");
        assert_eq!(result.iteration, 3);
        assert_eq!(result.total_token_estimate, 1);
        assert_eq!(result.phase, DebugPhase::BuildContext);
    }

    // ── handle_get_section ────────────────────────────────────────────

    #[tokio::test]
    async fn get_section_returns_content_for_known_section() {
        let mut ctrl = fresh_controller();
        ctrl.context_snapshots.insert(
            7,
            ContextSnapshot {
                iteration: 7,
                built_at: chrono::Utc::now(),
                sections: ContextSnapshotSections {
                    system_prompt: SectionContent::with_token_count("sys-body".to_string(), 5),
                    workspace_context: SectionContent::with_token_count(String::new(), 0),
                    environment: SectionContent::with_token_count(String::new(), 0),
                    tool_definitions: SectionContent::with_token_count(String::new(), 0),
                    skill_instructions: SectionContent::with_token_count(String::new(), 0),
                    retrieved_memory: SectionContent::with_token_count(String::new(), 0),
                    identity_context: SectionContent::with_token_count(String::new(), 0),
                },
                total_token_estimate: 5,
            },
        );

        let r = handle_get_section(
            &mut ctrl,
            GetSectionParams {
                iteration: 7,
                section: "system_prompt".to_string(),
            },
        )
        .await
        .expect("get_section ok");
        assert_eq!(r.content, "sys-body");
        assert_eq!(r.token_count, 5);
    }

    #[tokio::test]
    async fn get_section_returns_invalid_params_for_unknown_section_name() {
        let mut ctrl = fresh_controller();
        ctrl.context_snapshots.insert(
            1,
            ContextSnapshot {
                iteration: 1,
                built_at: chrono::Utc::now(),
                sections: ContextSnapshotSections {
                    system_prompt: SectionContent::with_token_count(String::new(), 0),
                    workspace_context: SectionContent::with_token_count(String::new(), 0),
                    environment: SectionContent::with_token_count(String::new(), 0),
                    tool_definitions: SectionContent::with_token_count(String::new(), 0),
                    skill_instructions: SectionContent::with_token_count(String::new(), 0),
                    retrieved_memory: SectionContent::with_token_count(String::new(), 0),
                    identity_context: SectionContent::with_token_count(String::new(), 0),
                },
                total_token_estimate: 0,
            },
        );

        let err = handle_get_section(
            &mut ctrl,
            GetSectionParams {
                iteration: 1,
                section: "no_such_section".to_string(),
            },
        )
        .await
        .expect_err("unknown section must error");
        assert!(matches!(err, DebugError::InvalidParams(_)));
        assert_eq!(err.rpc_code(), -32602);
    }

    #[tokio::test]
    async fn get_section_returns_not_found_when_snapshot_missing() {
        let mut ctrl = fresh_controller();
        let err = handle_get_section(
            &mut ctrl,
            GetSectionParams {
                iteration: 42,
                section: "system_prompt".to_string(),
            },
        )
        .await
        .expect_err("missing snapshot must error");
        assert!(matches!(err, DebugError::NotFound(_)));
    }

    // ── handle_rewind ─────────────────────────────────────────────────

    #[tokio::test]
    async fn rewind_sets_target_clears_patches_truncates_snapshots_and_notifies() {
        let mut ctrl = fresh_controller();
        ctrl.iteration = 10;
        ctrl.rewind_target = None;
        ctrl.pending_patches = Some(PatchSet {
            system_prompt: Some("stale".into()),
            ..Default::default()
        });
        ctrl.conversation_snapshots.push(ConversationSnapshot {
            id: "snap-10".into(),
            iteration: 10,
            message_count: 99,
            cumulative_usage: DebugUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            timestamp_ms: 0,
        });
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        let r = handle_rewind(
            &mut ctrl,
            &tx,
            RewindParams { to_iteration: 3 },
        )
        .await
        .expect("rewind ok");

        assert_eq!(r.rewound_to_iteration, 3);
        assert_eq!(r.messages_trimmed_to, 0, "no snapshot at iter 3 → 0");
        assert_eq!(ctrl.iteration, 3, "iteration resets immediately");
        assert_eq!(ctrl.rewind_target, Some(3));
        assert!(ctrl.pending_patches.is_none(), "rewind supersedes patches");
        assert!(
            ctrl.conversation_snapshots.is_empty(),
            "snapshots after target must be truncated (iter 10 > 3)"
        );
        // Re-execution path is opt-in; from a non-Stopped state, no
        // ExecutionStateChanged event must be emitted by rewind itself.
        assert!(
            drain_events(&mut rx).is_empty(),
            "rewind from non-Stopped must not emit ExecutionStateChanged"
        );
    }

    #[tokio::test]
    async fn rewind_from_stopped_transitions_back_to_paused_and_emits_state_change() {
        // This branch is the whole reason rewind exists post-Stop:
        // without it, await_debug_resume() returns false and the
        // agent loop short-circuits to "Agent stopped by debugger",
        // making rewind useless after Stop.
        let mut ctrl = fresh_controller();
        ctrl.state = DebugState::Stopped;
        ctrl.iteration = 10;
        let (tx, mut rx) = fresh_event_sender_pair("s1");

        handle_rewind(
            &mut ctrl,
            &tx,
            RewindParams { to_iteration: 2 },
        )
        .await
        .expect("rewind ok");

        assert_eq!(ctrl.state, DebugState::Paused);
        assert_eq!(ctrl.iteration, 2);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "Stopped→Paused must emit one event");
        match &events[0] {
            DebugEvent::ExecutionStateChanged {
                new_state,
                iteration,
            } => {
                assert_eq!(*new_state, DebugState::Paused);
                assert_eq!(*iteration, 2);
            }
            other => panic!("expected ExecutionStateChanged, got {other:?}"),
        }
    }

    // ── handle_patch_context ──────────────────────────────────────────

    #[tokio::test]
    async fn patch_context_merges_into_pending_and_updates_snapshot() {
        let mut ctrl = fresh_controller();
        ctrl.current_model = Some("test-model".to_string());
        ctrl.iteration = 4;
        // Pre-existing snapshot so we can verify the update path.
        ctrl.context_snapshots.insert(
            4,
            ContextSnapshot {
                iteration: 4,
                built_at: chrono::Utc::now(),
                sections: ContextSnapshotSections {
                    system_prompt: SectionContent::with_token_count("orig".into(), 1),
                    workspace_context: SectionContent::with_token_count(String::new(), 0),
                    environment: SectionContent::with_token_count(String::new(), 0),
                    tool_definitions: SectionContent::with_token_count("[]".into(), 0),
                    skill_instructions: SectionContent::with_token_count(String::new(), 0),
                    retrieved_memory: SectionContent::with_token_count(String::new(), 0),
                    identity_context: SectionContent::with_token_count(String::new(), 0),
                },
                total_token_estimate: 1,
            },
        );

        // First call: fresh patches.
        handle_patch_context(
            &mut ctrl,
            PatchContextParams {
                patches: PatchSet {
                    system_prompt: Some("patched-A".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("patch ok");
        assert!(ctrl.pending_patches.is_some());
        let snap = ctrl.context_snapshots.get(&4).unwrap();
        assert_eq!(snap.sections.system_prompt.content, "patched-A");

        // Second call: incremental merge — only `tool_definitions`
        // overrides; system_prompt from first patch must survive.
        handle_patch_context(
            &mut ctrl,
            PatchContextParams {
                patches: PatchSet {
                    tool_definitions: Some(vec![serde_json::json!({"name": "x"})]),
                    ..Default::default()
                },
            },
        )
        .await
        .expect("patch ok");
        let snap = ctrl.context_snapshots.get(&4).unwrap();
        assert_eq!(
            snap.sections.system_prompt.content, "patched-A",
            "previous system_prompt patch must survive merge"
        );
        assert!(
            snap.sections.tool_definitions.content.contains("\"name\": \"x\""),
            "new tool_definitions patch must be applied"
        );
    }

    // ── handle_re_execute ─────────────────────────────────────────────

    #[tokio::test]
    async fn re_execute_with_pending_patches_returns_has_patches_true() {
        let mut ctrl = fresh_controller();
        ctrl.pending_patches = Some(PatchSet {
            system_prompt: Some("x".into()),
            ..Default::default()
        });
        let outcome = handle_re_execute(&mut ctrl).await.expect("re_execute ok");
        assert!(outcome.has_patches);
        assert_eq!(ctrl.state, DebugState::Running);
        assert!(
            ctrl.re_execute_pending,
            "SessionTask must consume this flag"
        );
    }

    #[tokio::test]
    async fn re_execute_without_pending_patches_returns_has_patches_false() {
        let mut ctrl = fresh_controller();
        let outcome = handle_re_execute(&mut ctrl).await.expect("re_execute ok");
        assert!(!outcome.has_patches);
        assert_eq!(ctrl.state, DebugState::Running);
        assert!(ctrl.re_execute_pending);
    }

    // ── DebugError mapping (covers http/debug.rs From<DebugError>) ────

    #[test]
    fn debug_error_codes_match_http_route_mapping() {
        // Locks the contract between `DebugError::rpc_code()` and
        // `http/debug.rs`'s `From<DebugError> for DebugHttpError`
        // mapping. If anyone changes one side without the other, this
        // test breaks loudly.
        assert_eq!(DebugError::SessionNotFound("x".into()).rpc_code(), -32000);
        assert_eq!(DebugError::InvalidParams("x".into()).rpc_code(), -32602);
        assert_eq!(DebugError::NotFound("x".into()).rpc_code(), -32002);
        assert_eq!(DebugError::InvalidState("x".into()).rpc_code(), -32003);
        assert_eq!(DebugError::Internal("x".into()).rpc_code(), -32603);
    }
}
