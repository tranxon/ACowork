//! Concrete debug observer implementation backed by DebugController.
//!
//! [`DebugObserverImpl`] consolidates all debug logic that was previously
//! scattered across `loop_.rs`, `agent_core.rs`, and `session_task.rs`:
//!
//! - Phase tracking (`update_debug_phase`)
//! - Step event pushing (`push_debug_step`)
//! - Auto-pause in stepping mode (`debug_auto_pause_if_stepping`)
//! - Context snapshot capture (`capture_context_snapshot`)
//! - Pause/resume/rewind loop (`await_debug_resume`)
//! - Pending patches application
//! - Bypass injection (`check_and_apply_pending_debug`)

use std::sync::Arc;

use tokio::sync::Notify;

use super::controller::{
    ContextSnapshot, ContextSnapshotSections, DebugController, DebugState, SectionContent,
};
use super::observer::ContextSnapshotRequest;
use super::protocol::DebugPhase;
use super::server::{DebugEvent, DebugEventSender};
use super::DebugHandles;
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::session_state::SessionStatus;

// ── Debug Observer Implementation ─────────────────────────────────────

/// Real debug observer backed by DebugController, event sender, and notify handles.
///
/// This struct absorbs the 5 `Option<T>` fields that were previously on
/// `AgentCore` and the 5 debug methods that were on `AgentLoop`.
pub struct DebugObserverImpl {
    ctrl: Arc<tokio::sync::Mutex<DebugController>>,
    event_tx: DebugEventSender,
    rewind_notify: Arc<Notify>,
    resume_notify: Arc<Notify>,
    /// Pending debug handles injected by SessionManager while the agent loop
    /// is already running. The bypass injection path.
    pending_injection: Option<Arc<tokio::sync::Mutex<Option<DebugHandles>>>>,
}

impl DebugObserverImpl {
    /// Create a new DevMode observer from the given handles.
    pub fn new(handles: DebugHandles) -> Self {
        Self {
            ctrl: handles.debug_ctrl,
            event_tx: handles.debug_event_tx,
            rewind_notify: handles.rewind_notify,
            resume_notify: handles.resume_notify,
            pending_injection: None,
        }
    }

    /// Set the bypass injection channel (called by SessionManager).
    pub fn set_pending_injection(&mut self, ch: Arc<tokio::sync::Mutex<Option<DebugHandles>>>) {
        self.pending_injection = Some(ch);
    }

    /// Access the debug controller Arc.
    pub fn ctrl(&self) -> &Arc<tokio::sync::Mutex<DebugController>> {
        &self.ctrl
    }

    /// Access the rewind notify handle.
    pub fn rewind_notify(&self) -> &Arc<Notify> {
        &self.rewind_notify
    }

    /// Access the resume notify handle.
    pub fn resume_notify(&self) -> &Arc<Notify> {
        &self.resume_notify
    }

    /// Access the event sender.
    pub fn event_tx(&self) -> &DebugEventSender {
        &self.event_tx
    }

    /// Await debug resume: blocks if the debug controller is in Paused state.
    ///
    /// Uses `rewind_notify` via `tokio::select!` so that rewinds are applied
    /// immediately (notification-driven) rather than after up to 100ms of polling.
    ///
    /// Returns `true` if execution should continue, `false` if stopped.
    pub async fn await_resume(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        poll_stop: &mut dyn FnMut() -> bool,
        transition_status: &mut dyn FnMut(SessionStatus),
    ) -> bool {
        loop {
            // Check for Chat Panel STOP (arrives via inbound channel).
            if poll_stop() {
                tracing::info!("Debug: agent loop stopped via inbound channel");
                let mut ctrl_guard = self.ctrl.lock().await;
                let iteration = ctrl_guard.iteration;
                ctrl_guard.state = DebugState::Stopped;
                drop(ctrl_guard);
                let _ = self.event_tx.send(DebugEvent::ExecutionStateChanged {
                    new_state: DebugState::Stopped,
                    iteration,
                });
                return false;
            }

            // Consume any pending rewind target during polling.
            {
                let mut ctrl = self.ctrl.lock().await;
                apply_rewind_locked(&mut ctrl, session_id, history);
            }

            let state = {
                let ctrl = self.ctrl.lock().await;
                ctrl.state.clone()
            };
            match state {
                DebugState::Running => {
                    transition_status(SessionStatus::Streaming { message_id: None });
                    return true;
                }
                DebugState::Stepping => {
                    transition_status(SessionStatus::Streaming { message_id: None });
                    return true;
                }
                DebugState::Stopped => {
                    tracing::info!("Debug: agent loop stopped");
                    transition_status(SessionStatus::Idle);
                    return false;
                }
                DebugState::Paused => {
                    transition_status(SessionStatus::Paused {
                        iteration: None,
                        max_iterations: None,
                    });
                    // Use tokio::select! with rewind_notify so that
                    // rewinds are applied immediately (notification-driven)
                    // instead of waiting up to 100ms for the next poll.
                    tokio::select! {
                        _ = self.rewind_notify.notified() => {},
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                    }
                }
            }
        }
    }
}

// ── DebugObserver trait implementation ────────────────────────────────

impl super::observer::DebugObserver for DebugObserverImpl {
    // ── Iteration lifecycle ──

    fn on_iteration_start(&self, history_len: usize) -> Option<u32> {
        let Ok(mut ctrl) = self.ctrl.try_lock() else {
            return None;
        };

        ctrl.iteration += 1;
        let current_iter = ctrl.iteration;

        // Create conversation snapshot for rewind support.
        let usage = super::protocol::DebugUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        ctrl.create_conversation_snapshot(history_len, usage);

        Some(current_iter)
    }

    fn check_pending_injection(&self) {
        let Some(pending_arc) = self.pending_injection.as_ref() else {
            return;
        };

        let handles = match pending_arc.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };

        if let Some(_handles) = handles {
            // The bypass injection is handled externally —
            // SessionManager replaces the entire DebugObserverSlot.
            tracing::info!("DebugObserver: pending injection detected (bypass path)");
        }
    }

    // ── Pause / Resume / Rewind ──
    //
    // NOTE: `await_resume` is NOT on the trait because it needs &mut HistoryManager
    // which is owned by AgentLoop. It is called directly on DebugObserverImpl
    // (or via DebugObserverSlot::await_resume which delegates).

    async fn apply_rewind(&self, session_id: &str, history: &mut HistoryManager) {
        let mut ctrl = self.ctrl.lock().await;
        apply_rewind_locked(&mut ctrl, session_id, history);
    }

    async fn apply_rewind_and_patches(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        context_builder: &mut ContextBuilder,
    ) -> bool {
        let mut ctrl = self.ctrl.lock().await;

        // Apply rewind
        apply_rewind_locked(&mut ctrl, session_id, history);

        // Apply pending patches
        let mut patches_applied = false;
        if let Some(patches) = ctrl.pending_patches.take() {
            context_builder.apply_patches(&patches);
            tracing::info!(
                session_id = %session_id,
                "Debug: pending patches applied to context builder and consumed"
            );
            patches_applied = true;
        }

        // Consume re-execute pending flag
        if ctrl.take_re_execute_pending() {
            tracing::info!(
                session_id = %session_id,
                "Debug: re-execute flag consumed, agent will process next message with updated context"
            );
        }

        patches_applied
    }

    // ── Phase tracking ──

    async fn on_phase_enter(&self, phase: DebugPhase) -> bool {
        let mut ctrl_guard = self.ctrl.lock().await;
        let old_phase = ctrl_guard.phase;
        ctrl_guard.phase = phase;

        // Push state change event
        let _ = self.event_tx.send(DebugEvent::StateChanged {
            old_phase,
            new_phase: phase,
            iteration: ctrl_guard.iteration,
        });

        false
    }

    fn on_phase_step(
        &self,
        phase: DebugPhase,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
    ) {
        // Read iteration from controller (avoid holding lock across send).
        let iteration = {
            let Ok(ctrl) = self.ctrl.try_lock() else { return };
            ctrl.iteration
        };
        let _ = self.event_tx.send(DebugEvent::Step {
            iteration,
            phase,
            input,
            output,
            usage: None,
        });
    }

    async fn on_phase_step_done(&self) {
        let mut ctrl_guard = self.ctrl.lock().await;
        if ctrl_guard.state == DebugState::Stepping {
            ctrl_guard.state = DebugState::Paused;
            let iteration = ctrl_guard.iteration;
            drop(ctrl_guard);

            let _ = self.event_tx.send(DebugEvent::ExecutionStateChanged {
                new_state: DebugState::Paused,
                iteration,
            });
            tracing::info!("Debug: stepping complete, auto-pausing");
        }
    }

    // ── Context ──

    async fn on_context_built(&self, req: ContextSnapshotRequest<'_>) {
        let Some(iter) = req.iteration else {
            return;
        };

        // Build tool_definitions string: merge ContextBuilder's built-in tools
        // with MCP tools from all_tools.
        let mut all_defs: Vec<serde_json::Value> = req
            .context_builder
            .tool_definitions()
            .map(|defs| defs.to_vec())
            .unwrap_or_default();
        for tool in req.all_tools {
            let spec = tool.spec();
            if spec.name.starts_with("mcp:") {
                let val = serde_json::to_value(&spec).unwrap_or_default();
                all_defs.push(val);
            }
        }
        let tool_defs_str = serde_json::Value::Array(all_defs).to_string();

        let skill_str = req
            .context_builder
            .skill_instructions()
            .map(|s| s.to_string())
            .unwrap_or_default();

        tracing::info!(
            iter = iter,
            ws_has = req.context_builder.workspace_context().is_some(),
            ws_len = req.context_builder.workspace_context().map(|s| s.len()).unwrap_or(0),
            ws_preview = ?req.context_builder.workspace_context().map(|s| &s[..s.len().min(80)]),
            "capture_context_snapshot: workspace_context status"
        );

        let sections = ContextSnapshotSections {
            system_prompt: SectionContent::new(
                req.context_builder.system_prompt().to_string(),
                req.model,
            ),
            workspace_context: SectionContent::new(
                req.context_builder
                    .workspace_context()
                    .unwrap_or_default()
                    .to_string(),
                req.model,
            ),
            environment: SectionContent::new(
                req.context_builder
                    .environment_override()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| crate::agent::context::detect_environment_text()),
                req.model,
            ),
            tool_definitions: SectionContent::new(tool_defs_str, req.model),
            skill_instructions: SectionContent::new(skill_str, req.model),
            retrieved_memory: SectionContent::new(
                req.context_builder
                    .retrieved_memory()
                    .unwrap_or_default()
                    .to_string(),
                req.model,
            ),
            identity_context: SectionContent::new(
                req.context_builder
                    .identity_context()
                    .unwrap_or_default()
                    .to_string(),
                req.model,
            ),
        };

        let total_token_estimate = sections.system_prompt.token_estimate
            + sections.workspace_context.token_estimate
            + sections.environment.token_estimate
            + sections.tool_definitions.token_estimate
            + sections.skill_instructions.token_estimate
            + sections.retrieved_memory.token_estimate
            + sections.identity_context.token_estimate;

        let snapshot = ContextSnapshot {
            iteration: iter,
            built_at: chrono::Utc::now(),
            sections,
            total_token_estimate,
        };

        // Store in controller
        let mut ctrl_guard = self.ctrl.lock().await;
        ctrl_guard.current_model = Some(req.model.to_string());
        ctrl_guard.store_context_snapshot(snapshot.clone());

        // Push onContextBuilt event
        let context_sections = super::protocol::ContextSections::from(&snapshot.sections);
        let sent = self.event_tx.send(DebugEvent::ContextBuilt {
            iteration: snapshot.iteration,
            sections: context_sections,
            total_token_estimate: snapshot.total_token_estimate,
        });

        tracing::info!(
            iteration = snapshot.iteration,
            total_token_estimate,
            event_sent = sent,
            "Debug: context snapshot captured and event pushed"
        );
    }

    fn apply_pending_patches(&self, builder: &mut ContextBuilder) -> bool {
        let Ok(mut ctrl_guard) = self.ctrl.try_lock() else {
            return false;
        };
        if let Some(patches) = ctrl_guard.pending_patches.take() {
            builder.apply_patches(&patches);
            tracing::info!("Debug: pending patches applied to context builder after resume");
            true
        } else {
            false
        }
    }

    fn take_re_execute_pending(&self) -> bool {
        let Ok(mut ctrl_guard) = self.ctrl.try_lock() else {
            return false;
        };
        if ctrl_guard.take_re_execute_pending() {
            tracing::info!("Debug: re_execute_pending consumed inside agent loop after resume");
            true
        } else {
            false
        }
    }

    fn is_dev_mode(&self) -> bool {
        true
    }
}

// ── Rewind Logic ──────────────────────────────────────────────────────

/// Core rewind logic — assumes the caller already holds the controller lock.
///
/// Extracted into a lock-free helper so that rewind + patches + re-execute
/// can be applied within a single lock acquisition.
fn apply_rewind_locked(
    ctrl: &mut DebugController,
    session_id: &str,
    history: &mut HistoryManager,
) {
    if let Some(target_iter) = ctrl.take_rewind_target() {
        let msg_count = ctrl
            .conversation_snapshots
            .iter()
            .find(|s| s.iteration == target_iter)
            .map(|s| s.message_count);

        if let Some(count) = msg_count {
            history.truncate_to(count);
            tracing::info!(
                session_id = %session_id,
                target_iteration = target_iter,
                messages_trimmed_to = count,
                "Debug rewind: history truncated"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                target_iteration = target_iter,
                "Debug rewind: no snapshot found for target iteration, history unchanged"
            );
        }

        ctrl.iteration = target_iter;
        tracing::debug!(
            session_id = %session_id,
            target_iteration = target_iter,
            "Debug: rewind applied, iteration reset"
        );
    }
}
