//! Forgetting mechanism — decay, scan, and purge.
//!
//! Implements the multiplicative decay model for memory nodes (design §5.1):
//!   decay_score = importance * activity_signal
//!   activity_signal = clamp(recency_boost + access_boost, FLOOR, 1.0)
//!   recency_boost = exp(-lambda * days_since_last_access)
//!   access_boost = min(BOOST_CAP, access_count * ACCESS_PER_HIT)
//!
//! Sub-modules:
//! - `decay`: Pure decay-score calculation (uses the unified
//!   `acowork_memory::DecayConfig` as single source of truth).
//! - `scan`: Background scanning and state transitions (Active <-> Dormant).
//! - `purge_log`: Purge logging with 30-day recovery window.

pub mod decay;
pub mod purge_log;
pub mod scan;

pub use decay::{DecayConfig, compute_decay_score};
pub use purge_log::{PURGE_LOG_LABEL, PurgeLogEntry, PurgeReason};
