//! Forgetting mechanism — decay, scan, and purge.
//!
//! Implements the multiplicative decay model for memory nodes:
//!   decay_score = importance * activity_signal
//!   activity_signal = exp(-lambda * hours_since_last_access) + access_boost * recent_access_count
//!
//! Sub-modules:
//! - `decay`: Pure decay-score calculation.
//! - `scan`: Background scanning and state transitions (Active <-> Dormant).
//! - `purge_log`: Purge logging with 30-day recovery window.

pub mod decay;
pub mod purge_log;
pub mod scan;

pub use decay::{DecayConfig, compute_decay_score};
pub use purge_log::{PURGE_LOG_LABEL, PurgeLogEntry, PurgeReason};
