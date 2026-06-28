//! Event bus for the embed runtime — thin wrapper around acowork-core.
//!
//! Defines the embed-specific `State` enum and re-exports the generic
//! `EventBus<S>` and `BusEvent<S>` from `acowork_core::event_bus`.

use serde::Serialize;

/// High-level state of the embed runtime.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Process is starting up, no model loaded yet.
    Starting,
    /// Downloading the recommended model on first launch.
    DownloadingRecommended { model_id: String, progress: u8 },
    /// Loading a model from disk into ONNX Runtime.
    Loading { model_id: String },
    /// A model is loaded and serving inference requests.
    Ready { model_id: String, dimension: usize },
    /// Fatal error — no model is loaded and we cannot recover.
    Error { message: String },
}

// Re-export the generic event bus types, specialized for embed's State.
pub use acowork_core::event_bus::{BusEvent as Event, EventBus};
