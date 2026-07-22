//! Cancellation Token — session-level cancellation signal (single source of truth).
//!
//! See ADR-044 §4 for design rationale and §4.5 for phased rollout.
//!
//! Phase 1 introduces only the building blocks:
//! - [`CancellationToken`] — clonable handle to a shared cancellation state
//! - [`CancellationReason`] / [`StopSource`] — structured cancellation metadata
//! - [`select_cancelled`] — race a future against a token (drop-future semantics)
//!
//! Wiring into AgentLoop / SessionManager / MQTT dispatcher happens in
//! Phase 2 (additive) and Phase 3 (fixes the TTFT stop bug). Phase 4 cleans
//! up the legacy `urgent_stop` / `pending_interrupt` fields. This module
//! must remain pure and dependency-light so future phases can build on it.

mod reason;
mod token;
mod wrapper;

#[cfg(test)]
mod integration_tests;

pub use reason::{CancellationReason, StopSource};
pub use token::CancellationToken;
pub use wrapper::select_cancelled;
