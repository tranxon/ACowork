//! Event bus for the embed runtime.
//!
//! Publishes state transitions (model load/unload, download progress, errors)
//! and periodic heartbeats over a `tokio::sync::broadcast` channel. The
//! `/events` SSE endpoint subscribes to this bus and re-emits events to
//! connected gateway clients.
//!
//! Two event kinds share the same channel:
//!   - `Event::State` — published whenever embed's high-level state changes
//!   - `Event::Heartbeat` — published every 2s by the heartbeat task
//!
//! SSE consumers (gateway) treat missing heartbeats as the "process stuck"
//! signal. State events let the gateway learn the currently loaded model
//! without polling.

use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::time;

/// High-level state of the embed runtime.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Process is starting up, no model loaded yet.
    Starting,
    /// Downloading the recommended model on first launch.
    DownloadingRecommended {
        model_id: String,
        progress: u8,
    },
    /// Loading a model from disk into ONNX Runtime.
    Loading {
        model_id: String,
    },
    /// A model is loaded and serving inference requests.
    Ready {
        model_id: String,
        dimension: usize,
    },
    /// Fatal error — no model is loaded and we cannot recover.
    Error {
        message: String,
    },
}

impl State {
    /// Stable status string for the SSE `state` event payload.
    pub fn status(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::DownloadingRecommended { .. } => "downloading_recommended",
            State::Loading { .. } => "loading",
            State::Ready { .. } => "ready",
            State::Error { .. } => "error",
        }
    }
}

/// Event flowing over the bus. Both kinds carry an optional `seq` for
/// client-side ordering checks.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Periodic liveness signal.
    Heartbeat {
        seq: u64,
        ts_ms: u128,
    },
    /// High-level state transition.
    State {
        seq: u64,
        state: State,
    },
}

/// Bus for broadcasting events to all subscribers.
///
/// Cheap to clone (cheap to share via `Arc`). The underlying
/// `tokio::sync::broadcast` channel keeps a small ring of recent events
/// so late subscribers immediately receive the current state.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
    seq: Arc<AtomicU64>,
}

impl EventBus {
    /// Create a new bus. `buffer` is the per-subscriber ring size; late
    /// subscribers see at most this many past events.
    pub fn new(buffer: usize) -> Self {
        let (tx, _) = broadcast::channel(buffer);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Subscribe to the event stream. Returns a receiver that will see
    /// all events published from this point on (plus any buffered
    /// events that fit in the ring).
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    /// Publish a state transition. Returns the assigned sequence number.
    pub fn publish_state(&self, state: State) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let event = Arc::new(Event::State { seq, state });
        // Ignore "no active subscribers" error — we just drop the event.
        let _ = self.tx.send(event);
        seq
    }

    /// Publish a heartbeat. Internal — the heartbeat task calls this.
    fn publish_heartbeat(&self) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let event = Arc::new(Event::Heartbeat {
            seq,
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        });
        let _ = self.tx.send(event);
        seq
    }

    /// Spawn a background task that publishes a heartbeat every
    /// `interval_ms` milliseconds. The task ends when the bus is dropped
    /// (channel closed).
    pub fn spawn_heartbeat(&self, interval_ms: u64) {
        let bus = self.clone();
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(interval_ms));
            // Skip the immediate first tick so we don't fire instantly on spawn.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                bus.publish_heartbeat();
            }
        });
    }
}
