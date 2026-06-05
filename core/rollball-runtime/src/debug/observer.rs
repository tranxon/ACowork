//! Debug Observer — pluggable hook interface for DevMode.
//!
//! Provides [`DebugObserver`] trait and [`DebugObserverSlot`] enum dispatch
//! so that the agent loop can emit debug lifecycle events without scattering
//! `if let Some(ctrl)` guards throughout the business logic.
//!
//! ## Design
//!
//! - **Production mode**: [`DebugObserverSlot::Production`] — all methods are
//!   no-ops, and the compiler can eliminate dead code for zero runtime cost.
//! - **DevMode**: [`DebugObserverSlot::Dev`] wrapping [`DebugObserverImpl`],
//!   which delegates to the real [`DebugController`], [`DebugEventSender`],
//!   and notify handles.
//!
//! ## Why enum dispatch, not `Option<Box<dyn DebugObserver>>`?
//!
//! 1. Enum dispatch is static — the compiler can inline and dead-code-eliminate
//!    the `Production` variant.
//! 2. No heap allocation or vtable indirection.
//! 3. `match` branch prediction on modern CPUs is effectively free.
//! 4. Method signatures are visible for IDE completion.

use std::sync::Arc;

use rollball_core::tools::traits::Tool;
use tokio::sync::Notify;

use super::controller::DebugController;
use super::protocol::DebugPhase;
use super::server::DebugEventSender;
use super::DebugHandles;
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::session_state::SessionStatus;

// ── Debug Observer Trait ──────────────────────────────────────────────

/// Pluggable observer for agent loop lifecycle events.
///
/// In production mode, a no-op implementation is used (zero-cost abstraction
/// via enum dispatch, not dynamic dispatch). In DevMode, the real
/// [`DebugController`]-backed observer is injected.
///
/// All methods have default no-op implementations so that implementing
/// only the needed hooks is ergonomic.
///
/// ## Note on `await_resume`
///
/// `await_resume` is NOT part of this trait because it requires `&mut HistoryManager`,
/// which is owned by `AgentLoop`. Instead, it is a direct method on
/// [`DebugObserverImpl`] and delegated through [`DebugObserverSlot::await_resume`].
pub trait DebugObserver: Send + Sync {
    // ── Iteration lifecycle ──

    /// Called at the start of each iteration, before budget check.
    ///
    /// Returns the current debug iteration number (Some) if in DevMode,
    /// or None in production mode.
    fn on_iteration_start(&self, _history_len: usize) -> Option<u32> {
        None
    }

    /// Check for bypass-injected debug handles (called each iteration start).
    fn check_pending_injection(&self) {}

    // ── Rewind / Patches ──

    /// Apply any pending rewind operations to the history manager.
    async fn apply_rewind(&self, _session_id: &str, _history: &mut HistoryManager) {}

    /// Apply pending patches and rewind to the context builder.
    /// Returns true if patches were applied.
    async fn apply_rewind_and_patches(
        &self,
        _session_id: &str,
        _history: &mut HistoryManager,
        _context_builder: &mut ContextBuilder,
    ) -> bool {
        false
    }

    // ── Phase tracking ──

    /// Called when the agent loop enters a new phase.
    ///
    /// Updates the controller's phase, pushes a StateChanged event,
    /// checks breakpoints. Returns `true` if a breakpoint was hit.
    async fn on_phase_enter(&self, _phase: DebugPhase) -> bool {
        false
    }

    /// Called after a phase completes with its result.
    fn on_phase_step(&self, _phase: DebugPhase, _input: Option<serde_json::Value>, _output: Option<serde_json::Value>) {}

    /// Called after a phase step completes; auto-pauses if in stepping mode.
    async fn on_phase_step_done(&self) {}

    // ── Context ──

    /// Called after `ContextBuilder::build()` completes.
    async fn on_context_built(&self, _req: ContextSnapshotRequest<'_>) {}

    /// Apply any pending patches to the context builder.
    /// Returns true if patches were applied.
    fn apply_pending_patches(&self, _builder: &mut ContextBuilder) -> bool {
        false
    }

    /// Consume the re_execute_pending flag.
    fn take_re_execute_pending(&self) -> bool {
        false
    }

    // ── Observer access ──

    /// Returns true if this observer is backed by a real debug controller.
    fn is_dev_mode(&self) -> bool {
        false
    }
}

// ── Context Snapshot Request ──────────────────────────────────────────

/// Request payload for [`DebugObserver::on_context_built`].
///
/// Carries a reference to the ContextBuilder and the iteration/model info
/// needed to build a snapshot, without requiring the observer to know
/// about the full AgentLoop.
pub struct ContextSnapshotRequest<'a> {
    pub context_builder: &'a ContextBuilder,
    pub iteration: Option<u32>,
    pub model: &'a str,
    /// All tools (built-in + MCP) — needed for tool definitions snapshot.
    pub all_tools: &'a [Arc<dyn Tool>],
}

// ── Debug Observer Slot (Enum Dispatch) ───────────────────────────────

/// Slot that holds either a no-op observer (production) or a real one (DevMode).
///
/// Enum dispatch ensures zero overhead in production mode — the compiler
/// sees through the variant and eliminates dead code.
pub enum DebugObserverSlot {
    /// Production mode — all methods are no-ops.
    Production,
    /// DevMode — delegates to [`DebugObserverImpl`].
    Dev(super::observer_impl::DebugObserverImpl),
}

impl DebugObserverSlot {
    /// Create a production-mode slot (all no-ops).
    pub fn production() -> Self {
        DebugObserverSlot::Production
    }

    /// Create a DevMode slot wrapping the given implementation.
    pub fn dev(impl_: super::observer_impl::DebugObserverImpl) -> Self {
        DebugObserverSlot::Dev(impl_)
    }

    // ── Trait-delegated methods ──

    /// Delegate [`DebugObserver::on_iteration_start`].
    pub fn on_iteration_start(&self, history_len: usize) -> Option<u32> {
        match self {
            DebugObserverSlot::Production => None,
            DebugObserverSlot::Dev(obs) => obs.on_iteration_start(history_len),
        }
    }

    /// Delegate [`DebugObserver::check_pending_injection`].
    pub fn check_pending_injection(&self) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.check_pending_injection(),
        }
    }

    /// Delegate [`DebugObserver::apply_rewind`].
    pub async fn apply_rewind(&self, session_id: &str, history: &mut HistoryManager) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.apply_rewind(session_id, history).await,
        }
    }

    /// Delegate [`DebugObserver::apply_rewind_and_patches`].
    pub async fn apply_rewind_and_patches(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        context_builder: &mut ContextBuilder,
    ) -> bool {
        match self {
            DebugObserverSlot::Production => false,
            DebugObserverSlot::Dev(obs) => {
                obs.apply_rewind_and_patches(session_id, history, context_builder)
                    .await
            }
        }
    }

    /// Delegate [`DebugObserver::on_phase_enter`].
    pub async fn on_phase_enter(&self, phase: DebugPhase) -> bool {
        match self {
            DebugObserverSlot::Production => false,
            DebugObserverSlot::Dev(obs) => obs.on_phase_enter(phase).await,
        }
    }

    /// Delegate [`DebugObserver::on_phase_step`].
    pub fn on_phase_step(
        &self,
        phase: DebugPhase,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
    ) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.on_phase_step(phase, input, output),
        }
    }

    /// Delegate [`DebugObserver::on_phase_step_done`].
    pub async fn on_phase_step_done(&self) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.on_phase_step_done().await,
        }
    }

    /// Delegate [`DebugObserver::on_context_built`].
    pub async fn on_context_built(&self, req: ContextSnapshotRequest<'_>) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.on_context_built(req).await,
        }
    }

    /// Delegate [`DebugObserver::apply_pending_patches`].
    pub fn apply_pending_patches(&self, builder: &mut ContextBuilder) -> bool {
        match self {
            DebugObserverSlot::Production => false,
            DebugObserverSlot::Dev(obs) => obs.apply_pending_patches(builder),
        }
    }

    /// Delegate [`DebugObserver::take_re_execute_pending`].
    pub fn take_re_execute_pending(&self) -> bool {
        match self {
            DebugObserverSlot::Production => false,
            DebugObserverSlot::Dev(obs) => obs.take_re_execute_pending(),
        }
    }

    /// Delegate [`DebugObserver::is_dev_mode`].
    pub fn is_dev_mode(&self) -> bool {
        match self {
            DebugObserverSlot::Production => false,
            DebugObserverSlot::Dev(_) => true,
        }
    }

    // ── Non-trait methods (DevMode-only, production is no-op) ──

    /// Await debug resume: blocks if paused.
    ///
    /// NOT on the trait because it needs `&mut HistoryManager`.
    /// In production mode, returns `true` immediately.
    pub async fn await_resume(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        poll_stop: &mut dyn FnMut() -> bool,
        transition_status: &mut dyn FnMut(SessionStatus),
    ) -> bool {
        match self {
            DebugObserverSlot::Production => true,
            DebugObserverSlot::Dev(obs) => {
                obs.await_resume(session_id, history, poll_stop, transition_status)
                    .await
            }
        }
    }

    /// Access the rewind notify handle (DevMode only).
    pub fn rewind_notify(&self) -> Option<&Arc<Notify>> {
        match self {
            DebugObserverSlot::Production => None,
            DebugObserverSlot::Dev(obs) => Some(obs.rewind_notify()),
        }
    }

    /// Access the resume notify handle (DevMode only).
    pub fn resume_notify(&self) -> Option<&Arc<Notify>> {
        match self {
            DebugObserverSlot::Production => None,
            DebugObserverSlot::Dev(obs) => Some(obs.resume_notify()),
        }
    }

    /// Access the debug controller (DevMode only).
    pub fn debug_ctrl(&self) -> Option<&Arc<tokio::sync::Mutex<DebugController>>> {
        match self {
            DebugObserverSlot::Production => None,
            DebugObserverSlot::Dev(obs) => Some(obs.ctrl()),
        }
    }

    /// Access the event sender (DevMode only).
    pub fn debug_event_tx(&self) -> Option<&DebugEventSender> {
        match self {
            DebugObserverSlot::Production => None,
            DebugObserverSlot::Dev(obs) => Some(obs.event_tx()),
        }
    }

    /// Set the pending injection channel (DevMode only).
    /// No-op for Production.
    pub fn set_pending_injection(&mut self, ch: Arc<tokio::sync::Mutex<Option<DebugHandles>>>) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.set_pending_injection(ch),
        }
    }
}
