//! [`CancellationToken`] — Arc-shared handle to a session-level cancellation state.
//!
//! Design follows ADR-044 §4.1:
//! - State is a single `AtomicU8` (Active / Cancelled), flipped exactly once.
//! - Wakeups use `tokio::sync::Notify` (level-triggered — see tokio docs).
//! - The structured [`CancellationReason`](super::CancellationReason) is stored
//!   in a `Mutex<Option<_>>` and written only on the winning `cancel()` call.
//!
//! Thread-safety: every method is `&self` and safe to call from any task or
//! OS thread. Cloning is cheap (Arc clone) and all clones share the same
//! underlying state.

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::reason::CancellationReason;

// State values for `AtomicU8`. Use constants (not an enum) to keep the
// hot-path `load(Ordering::Acquire)` a single byte read.
const STATE_ACTIVE: u8 = 0;
const STATE_CANCELLED: u8 = 1;

/// Inner state shared by all clones of a [`CancellationToken`].
#[derive(Debug)]
struct CancellationInner {
    /// 0 = Active, 1 = Cancelled. Flipped exactly once via compare_exchange.
    state: AtomicU8,
    /// Level-triggered primitive — wakes all current waiters on cancel, and
    /// also resolves any future [`Notified`] created after the notification.
    notify: Notify,
    /// The structured reason supplied to the *first* `cancel()` call.
    /// Lock contention is expected to be near-zero: written once at cancel
    /// time, read only on the slow diagnostics path.
    reason: Mutex<Option<CancellationReason>>,
}

/// A clonable handle to a shared cancellation state.
///
/// Multiple holders observe the same state via cheap `Arc` clones. Any holder
/// can call [`cancel`](Self::cancel) to flip the state to `Cancelled` (idempotent)
/// and wake all current and future waiters of [`cancelled`](Self::cancelled).
///
/// # Lifecycle
/// - Initial state: `Active`
/// - After the *first* `cancel()`: `Cancelled` (state persists for the lifetime
///   of the `Arc`)
/// - Subsequent `cancel()` calls with different reasons are **silently ignored**
///   — first reason wins (see ADR §4.1 "first-wins" semantics; the original
///   reason is preserved for diagnostics).
///
/// # Cancellation race-freedom
/// The `cancelled()` future is safe to await before, during, or after a
/// `cancel()` call from any thread. We check the state *before* awaiting the
/// `Notify::notified()` future, which is sufficient because tokio's `Notify`
/// is level-triggered: once a notification has fired, subsequent `notified()`
/// futures resolve on first poll.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

impl CancellationToken {
    /// Create a new `Active` token.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationInner {
                state: AtomicU8::new(STATE_ACTIVE),
                notify: Notify::new(),
                reason: Mutex::new(None),
            }),
        }
    }

    /// Flip state to `Cancelled` and wake all current/future waiters.
    ///
    /// **First call wins**: if already `Cancelled`, this call is a no-op and
    /// returns `false`. The original reason is preserved.
    ///
    /// Returns `true` if this call performed the transition, `false` if the
    /// token was already cancelled (i.e. another holder called `cancel()` first).
    pub fn cancel(&self, reason: CancellationReason) -> bool {
        // AcqRel on success / Acquire on failure — pairs the write to `reason`
        // (under Mutex below) with the state flip.
        let prev = self.inner.state.compare_exchange(
            STATE_ACTIVE,
            STATE_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if prev.is_ok() {
            // First cancel — record reason and wake everyone.
            // Lock poisoning is unrecoverable for this primitive; we treat it
            // as a programmer error and panic rather than silently drop the
            // reason.
            *self
                .inner
                .reason
                .lock()
                .expect("CancellationToken reason mutex poisoned") = Some(reason);
            // notify_waiters wakes all currently-registered Notified futures;
            // any Notified future created after this point will resolve on
            // first poll because Notify is level-triggered.
            self.inner.notify.notify_waiters();
            true
        } else {
            // Already cancelled — drop this reason silently (first-wins).
            false
        }
    }

    /// Synchronous, non-blocking check.
    ///
    /// Hot-path safe: a single atomic load with `Acquire` ordering.
    pub fn is_cancelled(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == STATE_CANCELLED
    }

    /// Returns the cancellation reason if cancelled, else `None`.
    ///
    /// Holds a brief mutex; prefer [`is_cancelled`](Self::is_cancelled) on
    /// hot paths. Intended for diagnostics, telemetry, and user-facing error
    /// messages.
    pub fn reason(&self) -> Option<CancellationReason> {
        self.inner
            .reason
            .lock()
            .expect("CancellationToken reason mutex poisoned")
            .clone()
    }

    /// Future that resolves when the token is cancelled.
    ///
    /// Designed for use inside `tokio::select!`:
    ///
    /// ```ignore
    /// tokio::select! {
    ///     biased;
    ///     _ = token.cancelled() => { /* handle cancel */ }
    ///     event = stream.next() => { /* handle normal path */ }
    /// }
    /// ```
    ///
    /// Cancel-safe: dropping the future does not lose notifications, because
    /// the underlying [`Notify`] is level-triggered — a future dropped before
    /// resolving does not consume the notification; the next `cancelled()`
    /// future on the same token will resolve on first poll.
    ///
    /// If the token is already cancelled when this future is created (or first
    /// polled), the future resolves immediately without awaiting the notify.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        let token = self.clone();
        async move {
            // Fast path: already cancelled — no need to register with Notify.
            if token.is_cancelled() {
                return;
            }
            // Level-triggered: if cancel() races ahead of this await, the
            // Notified future resolves on first poll.
            token.inner.notify.notified().await;
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::cancellation::StopSource;

    #[test]
    fn new_is_active_and_has_no_reason() {
        let t = CancellationToken::new();
        assert!(!t.is_cancelled());
        assert!(t.reason().is_none());
    }

    #[test]
    fn cancel_returns_true_then_false() {
        let t = CancellationToken::new();
        assert!(t.cancel(CancellationReason::Pause));
        assert!(!t.cancel(CancellationReason::DebugStop));
        assert!(t.is_cancelled());
    }

    #[test]
    fn first_reason_wins_over_subsequent() {
        let t = CancellationToken::new();
        let first = CancellationReason::UserStop {
            source: StopSource::Cli,
            reason: "first".into(),
        };
        let second = CancellationReason::UserStop {
            source: StopSource::Cli,
            reason: "second".into(),
        };
        t.cancel(first.clone());
        t.cancel(second);
        assert_eq!(t.reason(), Some(first));
    }

    #[test]
    fn clones_share_state() {
        let a = CancellationToken::new();
        let b = a.clone();
        a.cancel(CancellationReason::SessionClosed);
        assert!(b.is_cancelled());
        assert_eq!(b.reason(), Some(CancellationReason::SessionClosed));
    }
}