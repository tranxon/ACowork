//! Debug protocol module for Agent Runtime DevMode.
//!
//! Provides:
//! - [`protocol`]: JSON-RPC 2.0 message types (request, response, notification)
//! - [`controller`]: DebugController — shared state (execution control, snapshots)
//! - [`server`]: WebSocket server (ws://127.0.0.1:19878) for Desktop App communication
//! - [`observer`]: [`DebugObserver`] trait + [`DebugObserverSlot`] enum dispatch
//! - [`observer_impl`]: [`DebugObserverImpl`] — concrete DevMode implementation
//!
//! The debug protocol follows Chrome DevTools Protocol (CDP) conventions
//! with JSON-RPC 2.0 over WebSocket. See `docs/design/10-debug-protocol.md`.

use std::sync::Arc;
use tokio::sync::Notify;

use crate::debug::controller::DebugController;
use crate::debug::server::DebugEventSender;

pub mod controller;
pub mod observer;
pub mod observer_impl;
pub mod protocol;
pub mod server;

// Re-export the primary types for convenience.
pub use observer::{ContextSnapshotRequest, DebugObserverSlot};
pub use observer_impl::DebugObserverImpl;

/// Bundle of debug-related handles injected into an AgentCore by SessionManager.
///
/// Each session gets its own independent instance so that debug state
/// (iteration counter, snapshots) is isolated per session.
#[derive(Clone)]
pub struct DebugHandles {
    pub debug_ctrl: Arc<tokio::sync::Mutex<DebugController>>,
    pub debug_event_tx: DebugEventSender,
    pub rewind_notify: Arc<Notify>,
    pub resume_notify: Arc<Notify>,
}
