//! Runtime process table + spawn/kill/reap + re-adopt (ADR-055 §6.20).
//!
//! **Migration landing point**: the Gateway's
//! `lifecycle/{manager,process}.rs` migrated here in Phase 2b, re-based
//! from `GatewayError`/`SharedState` to [`crate::error::NodeError`] /
//! [`crate::state::SharedNodeState`]. Sibling-binary + process-group +
//! reaper semantics unchanged.
//!
//! Re-adopt (§6.19): after a Node restart, reconcile the local process
//! table against the broker's retained `acowork/agents/{id}/status` and
//! the machine's process list (PID start-time guard against PID reuse).

pub mod manager;
pub mod reap;
pub mod spawn;

pub use manager::ProcessManager;
