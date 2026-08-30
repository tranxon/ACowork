//! ADR-059: Bootstrap capability-readiness aggregation.
//!
//! This module owns the Gateway's internal readiness picture. It does
//! NOT expose subsystem names or detail on the wire — every external
//! consumer sees only the aggregated `BootstrapPhase` plus the
//! protocol-level fields (`instance_id` / `version` / `phase_detail` /
//! `issued_at_ms`) defined in [`proto::acowork::mqtt::v1::BootstrapState`].
//!
//! Layout:
//! - [`registry`] — `SubsystemReadinessRegistry`: per-subsystem readiness
//!   state, the internal event bus, and the readiness handles that
//!   subsystems use to publish `ready` / `failed`.
//! - [`orchestrator`] — `BootstrapOrchestrator`: subscribes to the
//!   registry's events, recomputes the aggregated phase, and exposes
//!   the snapshot for the protocol publisher (Phase 1) and the HTTP
//!   `/api/bootstrap` projection (Phase 1.3).
//!
//! OCP boundary (ADR-059 §5.4): adding/removing/renaming a subsystem
//! MUST NOT change this module's public API or the wire-level types.
//! The registry accepts an arbitrary set of subsystem IDs at runtime;
//! the orchestrator only reasons about whether a subsystem is required
//! vs optional, not what it is named.

pub mod orchestrator;
pub mod registry;

pub use orchestrator::{BootstrapOrchestrator, BootstrapSnapshot};
pub use registry::{
    ReadinessKind, ReadinessUpdate, SharedSubsystemReadinessRegistry, SubsystemHandle,
    SubsystemId, SubsystemReadinessRegistry, SubsystemState,
};

/// Protocol version of the BootstrapState wire format.
///
/// Bumped on incompatible wire-format changes. Internal aggregation
/// logic is allowed to evolve freely without bumping this constant.
pub const BOOTSTRAP_PROTOCOL_VERSION: u32 = 1;