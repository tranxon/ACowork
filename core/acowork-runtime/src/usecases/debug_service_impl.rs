//! RuntimeDebugService — implements [`DebugService`].
//!
//! ADR-048: each trait method does exactly two things:
//! 1. Look up the per-session `DebugController` and `DebugEventSender`
//!    (acquiring locks).
//! 2. Call the corresponding `debug::handlers::*` business function.
//!
//! All business logic lives in `debug/handlers.rs` — this impl is a thin
//! locking + dispatch layer. The HTTP routes in `http/debug.rs` are the
//! only call site for the trait (ADR-048 D4 removed the legacy
//! `debug/server.rs` WebSocket server).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::debug::DebugEventSender;
use crate::debug::controller::DebugController;
use crate::debug::handlers::{DebugError, DebugStateSnapshot, ReExecuteOutcome, StepOutcome};
use crate::debug::protocol::{
    GetContextSnapshotParams, GetContextSnapshotResult, GetSectionParams, GetSectionResult,
    PatchContextParams, RewindParams, RewindResult, StepGranularity,
};

use super::debug_service::DebugService;

/// Concrete implementation of [`DebugService`] backed by per-session
/// `DebugController` instances owned by `SessionManager`.
///
/// The same `sessions` map is shared with `SessionManager::debug_controllers`.
/// When a new session is created under DevMode, SessionManager inserts a
/// fresh controller AND calls `register_event_sender`, and HTTP routes
/// (via this service) pick both up on the next request without any extra
/// wiring.
pub struct RuntimeDebugService {
    /// Per-session debug controllers (keyed by session_id).
    sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>>,
    /// Per-session event senders — each sender is a fresh `DebugEventSender`
    /// instance with the corresponding session_id embedded.
    event_senders: Arc<tokio::sync::RwLock<HashMap<String, DebugEventSender>>>,
}

impl RuntimeDebugService {
    /// Create a new `RuntimeDebugService` sharing the SessionManager's
    /// per-session state.
    pub fn new(
        sessions: Arc<
            tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>,
        >,
        event_senders: Arc<tokio::sync::RwLock<HashMap<String, DebugEventSender>>>,
    ) -> Self {
        Self {
            sessions,
            event_senders,
        }
    }

    /// Look up the per-session controller and event sender, returning
    /// `SessionNotFound` if either is missing.
    async fn get_session_state(
        &self,
        session_id: &str,
    ) -> Result<(Arc<tokio::sync::Mutex<DebugController>>, DebugEventSender), DebugError> {
        let ctrl = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        }
        .ok_or_else(|| DebugError::SessionNotFound(session_id.to_string()))?;

        let tx = {
            let senders = self.event_senders.read().await;
            senders.get(session_id).cloned()
        }
        .ok_or_else(|| DebugError::SessionNotFound(session_id.to_string()))?;

        Ok((ctrl, tx))
    }

    /// Register an event sender for a specific session. Called by
    /// `SessionManager` when a new session is created under DevMode.
    pub async fn register_event_sender(&self, session_id: &str, sender: DebugEventSender) {
        let mut senders = self.event_senders.write().await;
        senders.insert(session_id.to_string(), sender);
    }
}

#[async_trait]
impl DebugService for RuntimeDebugService {
    async fn resume(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_resume(&mut ctrl, &tx).await
    }

    async fn pause(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_pause(&mut ctrl, &tx).await
    }

    async fn step(
        &self,
        session_id: &str,
        granularity: StepGranularity,
    ) -> Result<StepOutcome, DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_step(&mut ctrl, &tx, granularity).await
    }

    async fn stop(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_stop(&mut ctrl, &tx).await
    }

    async fn get_state(&self, session_id: &str) -> Result<DebugStateSnapshot, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_state(&mut ctrl).await
    }

    async fn get_context_snapshot(
        &self,
        session_id: &str,
        params: GetContextSnapshotParams,
    ) -> Result<GetContextSnapshotResult, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_context_snapshot(&mut ctrl, params).await
    }

    async fn get_section(
        &self,
        session_id: &str,
        params: GetSectionParams,
    ) -> Result<GetSectionResult, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_section(&mut ctrl, params).await
    }

    async fn rewind(
        &self,
        session_id: &str,
        params: RewindParams,
    ) -> Result<RewindResult, DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_rewind(&mut ctrl, &tx, params).await
    }

    async fn patch_context(
        &self,
        session_id: &str,
        params: PatchContextParams,
    ) -> Result<(), DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_patch_context(&mut ctrl, params).await
    }

    async fn re_execute(&self, session_id: &str) -> Result<ReExecuteOutcome, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_re_execute(&mut ctrl).await
    }
}
