//! Session config module (ADR-047).
//!
//! Decouples session config persistence from the LLM inference loop.
//! - `delta::SessionConfigDelta` -- partial config update, the single payload
//!   for all config mutations.
//! - `delta::SessionConfigSnapshot` -- read-only snapshot of current config.
//! - `llm_effects` -- deferred LLM-side effects applied at turn boundaries.

pub mod delta;
pub mod llm_effects;

pub use delta::{SessionConfigDelta, SessionConfigSnapshot};
