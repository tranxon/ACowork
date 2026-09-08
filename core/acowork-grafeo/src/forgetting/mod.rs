//! Forgetting — episodic time decay and purge log.
//!
//! Episodic nodes age on a half-life retention curve
//! (`exp(-ln2 × age_days / half_life_days)`, opt-in via
//! [`acowork_memory::EpisodicDecayConfig`]): Active → Dormant below the
//! dormant threshold, Dormant → PurgeLog archive after `archive_days`.
//! The semantic layer (Knowledge / Procedural / Autobiographical) never
//! decays.
//!
//! Sub-modules:
//! - `episodic_decay`: the scan engine (sole consumer of `transition_to_dormant`).
//! - `purge_log`: archive records with a 30-day recovery window.

pub mod episodic_decay;
pub mod purge_log;

pub use purge_log::{PURGE_LOG_LABEL, PurgeLogEntry, PurgeReason};
