//! Consolidation scheduler — legacy Pending-count scheduler.
//!
//! The original `ConsolidationScheduler` (idle-timeout / accumulation /
//! manual triggers over a Pending-node count) was DELETED as dead code
//! (ADR-057 §5.3 redesign, bugfix/memory):
//!
//! - Pending nodes have had no producer since ADR-068 (memory_store writes
//!   episodes; the EpisodicDistiller promotes straight to Active), so the
//!   accumulation trigger could never fire.
//! - The Runtime owns scheduling through
//!   [`acowork_runtime::memory::consolidation_bg::ConsolidationTimer`]
//!   (ADR-051 P4) with its own trigger set (distiller / forgetting
//!   intervals) — this crate's scheduler was never instantiated.
//!
//! Only the [`SchedulerConfig`] re-export survives for API stability
//! (the type itself lives in `acowork-memory` and is still the config
//! source for the Runtime's consolidation loop).

// ---------------------------------------------------------------------------
// Configuration (re-exported from acowork-memory)
// ---------------------------------------------------------------------------

pub use acowork_memory::consolidation::SchedulerConfig;
