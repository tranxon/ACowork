//! Session state machine (ADR-039 §5.1).
//!
//! Provides a `tokio::sync::watch`-based broadcast so that external
//! consumers (Tauri events, DevMode, health ledger) can observe
//! the MQTT connection lifecycle in real time.

use std::sync::Arc;

use tokio::sync::watch;

/// The lifecycle state of an MQTT session.
///
/// ```mermaid
/// graph LR
///     Idle --> Connecting
///     Connecting --> Connected
///     Connecting --> Disconnected
///     Connected --> Reconnecting
///     Reconnecting --> Connected
///     Reconnecting --> Disconnected
///     Connected --> Disconnected
///     Disconnected --> Connecting
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// No connection attempt has been made yet.
    Idle,

    /// `AsyncClient` created, waiting for the first `ConnAck`.
    Connecting,

    /// `ConnAck` received, bootstrap completed. Ready for
    /// publish/subscribe.
    Connected,

    /// Was connected, lost the connection, waiting for rumqttc's
    /// automatic reconnect to deliver a new `ConnAck`.
    Reconnecting,

    /// Permanently disconnected (fatal error or explicit shutdown).
    /// No further reconnect attempts will be made.
    Disconnected {
        /// Human-readable reason for disconnection.
        reason: String,
    },
}

impl SessionState {
    /// Returns `true` if the session is in a connected state and
    /// ready for publish/subscribe.
    pub fn is_connected(&self) -> bool {
        matches!(self, SessionState::Connected)
    }

    /// Returns `true` if the session is in a transient (non-terminal)
    /// state that may progress to `Connected`.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            SessionState::Idle | SessionState::Connecting | SessionState::Reconnecting
        )
    }

    /// Returns `true` if the session is permanently stopped.
    pub fn is_terminal(&self) -> bool {
        matches!(self, SessionState::Disconnected { .. })
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Idle => write!(f, "idle"),
            SessionState::Connecting => write!(f, "connecting"),
            SessionState::Connected => write!(f, "connected"),
            SessionState::Reconnecting => write!(f, "reconnecting"),
            SessionState::Disconnected { reason } => {
                write!(f, "disconnected ({reason})")
            }
        }
    }
}

// ── watch channel wrappers ───────────────────────────────────

/// Sender side of the session-state broadcast.
///
/// Held by the event loop. Cloning is cheap (Arc inside).
#[derive(Clone)]
pub struct SessionStateTx {
    tx: watch::Sender<SessionState>,
}

impl SessionStateTx {
    /// Create a new broadcast channel with the given initial state.
    pub fn new(initial: SessionState) -> (Self, SessionStateRx) {
        let (tx, rx) = watch::channel(initial);
        (
            SessionStateTx { tx },
            SessionStateRx {
                rx: Arc::new(Mutex::new(rx)),
            },
        )
    }

    /// Update the state. Logs the transition at INFO level.
    pub fn set(&self, state: SessionState) {
        let prev = self.tx.borrow().clone();
        if prev != state {
            tracing::info!(from = %prev, to = %state, "MQTT session state transition");
        }
        let _ = self.tx.send(state);
    }

    /// Read the current state without subscribing.
    pub fn current(&self) -> SessionState {
        self.tx.borrow().clone()
    }

    /// Create a new receiver that can observe future state changes.
    /// The receiver will see the current state immediately on first
    /// `changed()` call.
    pub fn subscribe(&self) -> SessionStateRx {
        SessionStateRx {
            rx: Arc::new(Mutex::new(self.tx.subscribe())),
        }
    }
}

/// Receiver side of the session-state broadcast.
///
/// Held by external consumers (Tauri command handlers, DevMode,
/// health ledger). Cloning is NOT supported – wrap in `Arc` if
/// multiple consumers need the same receiver, or create multiple
/// receivers from the same `SessionStateTx` (not yet supported –
/// use `tokio::sync::broadcast` if fan-out is needed).
pub struct SessionStateRx {
    rx: Arc<Mutex<watch::Receiver<SessionState>>>,
}

use std::sync::Mutex;

impl SessionStateRx {
    /// Wait for the next state change, returning the new state.
    ///
    /// Returns `Err` if all senders have been dropped.
    pub async fn changed(&self) -> Result<SessionState, watch::error::SendError<SessionState>> {
        let mut rx = {
            // Clone the receiver out of the mutex so we don't hold
            // the lock across the `.await` below (clippy:
            // await_holding_lock).
            self.rx.lock().unwrap().clone()
        };
        rx.changed().await.map_err(|_| {
            watch::error::SendError(SessionState::Disconnected {
                reason: "sender dropped".into(),
            })
        })?;
        Ok(rx.borrow().clone())
    }

    /// Read the current state without waiting.
    pub fn current(&self) -> SessionState {
        self.rx.lock().unwrap().borrow().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions() {
        assert!(SessionState::Idle.is_transient());
        assert!(!SessionState::Idle.is_connected());

        assert!(SessionState::Connected.is_connected());
        assert!(!SessionState::Connected.is_transient());

        assert!(SessionState::Reconnecting.is_transient());
        assert!(!SessionState::Reconnecting.is_connected());

        let d = SessionState::Disconnected {
            reason: "test".into(),
        };
        assert!(d.is_terminal());
        assert!(!d.is_connected());
        assert!(!d.is_transient());
    }

    #[test]
    fn display_formats() {
        assert_eq!(SessionState::Idle.to_string(), "idle");
        assert_eq!(SessionState::Connected.to_string(), "connected");
        assert_eq!(
            SessionState::Disconnected {
                reason: "boom".into()
            }
            .to_string(),
            "disconnected (boom)"
        );
    }

    #[tokio::test]
    async fn watch_channel_delivers_changes() {
        let (tx, rx) = SessionStateTx::new(SessionState::Idle);
        assert_eq!(rx.current(), SessionState::Idle);

        tx.set(SessionState::Connecting);
        // Give the watch channel a tick to propagate.
        tokio::task::yield_now().await;

        let state = rx.changed().await.unwrap();
        assert_eq!(state, SessionState::Connecting);

        tx.set(SessionState::Connected);
        let state = rx.changed().await.unwrap();
        assert_eq!(state, SessionState::Connected);
    }

    #[tokio::test]
    async fn watch_channel_current_is_instant() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connected);
        assert_eq!(tx.current(), SessionState::Connected);
        assert_eq!(rx.current(), SessionState::Connected);
    }
}
