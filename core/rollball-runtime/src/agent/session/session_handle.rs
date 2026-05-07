//! SessionHandle: external handle for interacting with a SessionTask.
//!
//! Provides a typed interface for sending messages to a session and
//! checking whether the session task is still alive.

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::session_task::SessionMessage;

/// External handle for interacting with a running SessionTask.
///
/// Callers (e.g., Gateway, Desktop App) use this handle to send
/// messages to a specific session without needing direct access to
/// the SessionTask or its AgentLoop.
pub struct SessionHandle {
    /// Unique session identifier
    pub session_id: String,
    /// Channel for sending messages to the SessionTask
    pub(crate) inbound_tx: mpsc::Sender<SessionMessage>,
    /// Join handle for the session's tokio task (for lifecycle observation)
    pub(crate) join_handle: JoinHandle<()>,
}

impl SessionHandle {
    /// Send a message to this session.
    ///
    /// Returns an error if the session task has stopped and the channel
    /// is closed, or if the channel is full.
    pub fn send(&self, msg: SessionMessage) -> Result<(), Box<tokio::sync::mpsc::error::TrySendError<SessionMessage>>> {
        self.inbound_tx.try_send(msg).map_err(Box::new)
    }

    /// Check whether the session task is still running.
    ///
    /// Returns `false` if the JoinHandle has been consumed (task completed)
    /// or if the inbound channel is closed.
    pub fn is_alive(&self) -> bool {
        !self.join_handle.is_finished() && !self.inbound_tx.is_closed()
    }
}
