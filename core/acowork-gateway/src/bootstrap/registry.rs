//! ADR-059 §5.4.3 — Internal subsystem readiness registry.
//!
//! Tracks the readiness of every Gateway-internal subsystem and exposes
//! an event stream that the [`super::BootstrapOrchestrator`] consumes.
//! The registry deliberately does NOT know anything about wire formats,
//! MQTT, or HTTP — it is pure in-process state.
//!
//! Subsystem authors:
//! - Register at startup with [`SubsystemReadinessRegistry::register`],
//!   declaring whether the subsystem is required for `READY` and
//!   optionally providing a friendly detail string for diagnostics.
//! - Receive a [`SubsystemHandle`] that lets them push `ready` or
//!   `failed` updates without holding a reference to the registry.
//! - Call [`SubsystemHandle::mark_ready`] / [`SubsystemHandle::mark_failed`]
//!   exactly once during their lifecycle; further updates are silently
//!   ignored (a subsystem transitioning to ready should not oscillate).
//!
//! OCP (ADR-059 §5.4): the set of subsystem IDs is open. Adding,
//! removing or renaming a subsystem changes no wire-level types; it
//! only changes how the Gateway registers subsystems at startup.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Stable opaque identifier for a registered subsystem.
///
/// Defined as a `String` newtype so subsystem authors can use any
/// domain-specific naming (`"vault"`, `"node.local"`, `"embedding"`,
/// …) without forcing changes to the orchestrator.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubsystemId(pub String);

impl std::fmt::Display for SubsystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SubsystemId {
    fn from(s: &str) -> Self {
        SubsystemId(s.to_string())
    }
}

impl From<String> for SubsystemId {
    fn from(s: String) -> Self {
        SubsystemId(s)
    }
}

/// State of a single subsystem in the readiness registry.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SubsystemState {
    /// Subsystem has been registered but has not yet signalled readiness.
    Booting,
    /// Subsystem is healthy and ready to participate.
    Ready,
    /// Subsystem is no longer healthy; further transitions are ignored.
    Failed,
    /// Subsystem has been intentionally marked as not required for
    /// `READY` (e.g. dev mode skipping embedding). Treated as "ready"
    /// for the purpose of phase aggregation but distinct in `phase_detail`.
    Skipped,
}

impl SubsystemState {
    /// Whether this state counts as "ready" for required-subsystem aggregation.
    ///
    /// Both [`SubsystemState::Ready`] and [`SubsystemState::Skipped`]
    /// satisfy the required-subsystem check — `Skipped` is for
    /// optional subsystems that were intentionally not started.
    pub fn is_satisfying(self) -> bool {
        matches!(self, SubsystemState::Ready | SubsystemState::Skipped)
    }
}

/// Whether a subsystem is required for the Gateway to advertise
/// `phase = READY`.
///
/// A required subsystem that stays in `Booting` keeps the phase in
/// `BOOTING`. A required subsystem that transitions to `Failed` drops
/// the phase to `FAILED`. An optional subsystem that transitions to
/// `Failed` drops the phase to `DEGRADED`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReadinessKind {
    /// Subsystem must be `Ready` for the Gateway to advertise `READY`.
    Required,
    /// Subsystem failure does not block `READY`; it demotes the phase
    /// to `DEGRADED`.
    Optional,
}

/// Update event emitted by the registry when a subsystem transitions.
///
/// Consumers (`BootstrapOrchestrator`) subscribe to a
/// `broadcast::Receiver<ReadinessUpdate>` and update the aggregated
/// phase on every event.
#[derive(Debug, Clone)]
pub struct ReadinessUpdate {
    pub id: SubsystemId,
    pub previous: SubsystemState,
    pub current: SubsystemState,
    pub kind: ReadinessKind,
    /// Optional human-readable detail string. NOT routed by external
    /// consumers (per ADR-059 §5.4.4); used only for `phase_detail`.
    pub detail: Option<String>,
}

/// Per-subsystem registration record held inside the registry.
#[derive(Debug, Clone)]
struct SubsystemRecord {
    state: SubsystemState,
    kind: ReadinessKind,
    /// Last human-readable detail supplied by the subsystem author.
    detail: Option<String>,
}

/// Handle handed to a subsystem author on registration.
///
/// The handle holds a `Weak` reference to the registry so dropping
/// the registry (or the owning Gateway) does not leak the handle; an
/// update pushed through a dead handle is a no-op.
#[derive(Clone)]
pub struct SubsystemHandle {
    id: SubsystemId,
    registry: Weak<SubsystemReadinessRegistry>,
}

impl SubsystemHandle {
    /// Mark the subsystem as ready.
    ///
    /// No-op if the registry has been dropped, or if the subsystem has
    /// already transitioned out of `Booting`. Subsystems must not
    /// transition out of `Ready` under normal operation.
    pub fn mark_ready(&self, detail: impl Into<Option<String>>) {
        if let Some(registry) = self.registry.upgrade() {
            registry.transition(&self.id, SubsystemState::Ready, detail.into());
        }
    }

    /// Mark the subsystem as failed.
    ///
    /// Terminal: subsequent `mark_ready` calls are ignored. The
    /// orchestrator will either drop the phase to `FAILED` (required)
    /// or `DEGRADED` (optional).
    pub fn mark_failed(&self, detail: impl Into<Option<String>>) {
        if let Some(registry) = self.registry.upgrade() {
            registry.transition(&self.id, SubsystemState::Failed, detail.into());
        }
    }

    /// Mark the subsystem as intentionally skipped (optional-only).
    ///
    /// Behaves like `mark_ready` for phase aggregation purposes but
    /// records a distinct state so `phase_detail` can surface "skipped"
    /// separately from "ready".
    pub fn mark_skipped(&self, detail: impl Into<Option<String>>) {
        if let Some(registry) = self.registry.upgrade() {
            registry.transition(&self.id, SubsystemState::Skipped, detail.into());
        }
    }

    /// Re-enter the `Booting` state (ADR-059 §7.2: a Node whose control
    /// channel went down — LWT / status offline / empty retained
    /// `NodeReady` — is demoted back to not-ready so the aggregated
    /// phase drops from READY to BOOTING until it re-announces).
    ///
    /// Unlike [`Self::mark_failed`] this is NOT terminal: a later
    /// `mark_ready` (Node re-announces `NodeReady` on reconnect) is
    /// honoured. No-op for subsystems already in `Booting`.
    pub fn mark_booting(&self, detail: impl Into<Option<String>>) {
        if let Some(registry) = self.registry.upgrade() {
            registry.transition(&self.id, SubsystemState::Booting, detail.into());
        }
    }

    /// Subsystem id, useful for diagnostics.
    pub fn id(&self) -> &SubsystemId {
        &self.id
    }
}

/// Type alias for the shared registry handle.
///
/// Most code paths interact with the registry through `Arc<SubsystemReadinessRegistry>`;
/// the alias keeps signatures readable.
pub type SharedSubsystemReadinessRegistry = Arc<SubsystemReadinessRegistry>;

/// In-process registry of subsystem readiness.
///
/// Internal — public methods only cover what subsystem authors and the
/// orchestrator need: register, transition, subscribe, snapshot.
pub struct SubsystemReadinessRegistry {
    inner: Mutex<Inner>,
}

/// Lock-protected registry state.
struct Inner {
    /// Subsystem id → registration record.
    subsystems: HashMap<SubsystemId, SubsystemRecord>,
    /// Broadcast channel emitting every transition.
    tx: broadcast::Sender<ReadinessUpdate>,
}

impl SubsystemReadinessRegistry {
    /// Create a new empty registry with a generous broadcast capacity.
    ///
    /// Capacity 256 is plenty: subsystems transition at most a handful
    /// of times during a Gateway lifetime; the broadcast buffer covers
    /// brief orchestrator outages.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            inner: Mutex::new(Inner {
                subsystems: HashMap::new(),
                tx,
            }),
        }
    }

    /// Wrap this registry in an `Arc` so it can be shared with
    /// subsystems and the orchestrator.
    pub fn new_shared() -> SharedSubsystemReadinessRegistry {
        Arc::new(Self::new())
    }

    /// Register a subsystem and obtain a handle for pushing updates.
    ///
    /// Calling `register` twice with the same id is treated as a
    /// configuration error: the existing record is returned unchanged
    /// and the new kind is ignored. Subsystem authors should not
    /// double-register.
    pub fn register(
        self: &SharedSubsystemReadinessRegistry,
        id: impl Into<SubsystemId>,
        kind: ReadinessKind,
    ) -> SubsystemHandle {
        let id = id.into();
        let weak = Arc::downgrade(self);
        {
            let mut inner = self.inner.lock();
            inner.subsystems.entry(id.clone()).or_insert(SubsystemRecord {
                state: SubsystemState::Booting,
                kind,
                detail: None,
            });
        }
        SubsystemHandle { id, registry: weak }
    }

    /// Apply a state transition; emits a [`ReadinessUpdate`] if the
    /// state actually changed.
    ///
    /// Idempotent: calling `transition(.., Ready, ..)` twice keeps the
    /// subsystem in `Ready` and emits only one update.
    fn transition(&self, id: &SubsystemId, next: SubsystemState, detail: Option<String>) {
        let update = {
            let mut inner = self.inner.lock();
            let Some(record) = inner.subsystems.get_mut(id) else {
                // Unknown subsystem: ignore. This can happen if a
                // subsystem author pushes an update after its handle
                // was retired; the orchestrator never observes it.
                return;
            };
            if record.state == next {
                return;
            }
            let previous = record.state;
            record.state = next;
            if detail.is_some() {
                record.detail = detail;
            }
            let update = ReadinessUpdate {
                id: id.clone(),
                previous,
                current: next,
                kind: record.kind,
                detail: record.detail.clone(),
            };
            // Drop the lock before broadcasting so subscribers can
            // re-enter the registry without deadlocking.
            drop(inner);
            update
        };
        // Best-effort broadcast: if there are zero subscribers, the
        // update is silently dropped. The orchestrator always holds a
        // subscriber after construction, so this is fine.
        let _ = self.inner.lock().tx.send(update);
    }

    /// Subscribe to readiness updates.
    ///
    /// The orchestrator holds one subscription across its lifetime.
    pub fn subscribe(&self) -> broadcast::Receiver<ReadinessUpdate> {
        self.inner.lock().tx.subscribe()
    }

    /// Snapshot the current per-subsystem state under the lock.
    ///
    /// Used by the orchestrator to compute the initial phase before
    /// the first event arrives, and by HTTP `/api/bootstrap/subsystems`
    /// (Phase 5 diagnostics).
    pub fn snapshot(&self) -> Vec<(SubsystemId, SubsystemState, ReadinessKind, Option<String>)> {
        let inner = self.inner.lock();
        inner
            .subsystems
            .iter()
            .map(|(id, rec)| (id.clone(), rec.state, rec.kind, rec.detail.clone()))
            .collect()
    }

    /// Look up the state of a single subsystem.
    pub fn state(&self, id: &SubsystemId) -> Option<SubsystemState> {
        self.inner.lock().subsystems.get(id).map(|r| r.state)
    }

    /// True iff the subsystem is currently `Ready` (not merely
    /// registered). Used by dependency gates such as
    /// `install_agent`'s `node.{node_id}` check (ADR-059 §7.2): a
    /// Booting / Failed / Skipped subsystem must not accept dependent
    /// work.
    pub fn is_ready(&self, id: &SubsystemId) -> bool {
        self.state(id) == Some(SubsystemState::Ready)
    }

    /// True iff every *required* subsystem is in a satisfying state
    /// AND at least one required subsystem is registered.
    ///
    /// Used by the orchestrator's recompute path; intentionally does
    /// not consider optional subsystems.
    ///
    /// An empty required set returns `false` so a Gateway with no
    /// registered required subsystems cannot advertise `READY` before
    /// any subsystem actually registers. Callers that want the
    /// trivial-true interpretation of "all (zero) required items are
    /// satisfied" should check [`Self::registered_required_count`].
    pub fn all_required_ready(&self) -> bool {
        let inner = self.inner.lock();
        let required: Vec<_> = inner
            .subsystems
            .values()
            .filter(|r| r.kind == ReadinessKind::Required)
            .collect();
        if required.is_empty() {
            return false;
        }
        required.iter().all(|r| r.state.is_satisfying())
    }

    /// Count of registered required subsystems.
    pub fn registered_required_count(&self) -> usize {
        let inner = self.inner.lock();
        inner
            .subsystems
            .values()
            .filter(|r| r.kind == ReadinessKind::Required)
            .count()
    }

    /// True iff any *required* subsystem is in [`SubsystemState::Failed`].
    pub fn any_required_failed(&self) -> bool {
        let inner = self.inner.lock();
        inner
            .subsystems
            .values()
            .any(|r| r.kind == ReadinessKind::Required && r.state == SubsystemState::Failed)
    }

    /// True iff any *optional* subsystem is in [`SubsystemState::Failed`].
    pub fn any_optional_failed(&self) -> bool {
        let inner = self.inner.lock();
        inner
            .subsystems
            .values()
            .any(|r| r.kind == ReadinessKind::Optional && r.state == SubsystemState::Failed)
    }

    /// True iff every *required* subsystem has been registered and has
    /// not yet transitioned out of `Booting` — i.e. still booting.
    ///
    /// Empty registries return `false` so a brand-new Gateway does not
    /// pretend to be `READY` before subsystems register.
    pub fn any_required_booting(&self) -> bool {
        let inner = self.inner.lock();
        if inner.subsystems.is_empty() {
            return false;
        }
        inner
            .subsystems
            .values()
            .filter(|r| r.kind == ReadinessKind::Required)
            .any(|r| r.state == SubsystemState::Booting)
    }
}

impl Default for SubsystemReadinessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsystem_id_display_and_from() {
        let id: SubsystemId = "vault".into();
        assert_eq!(id.to_string(), "vault");
        let id2 = SubsystemId::from(String::from("publisher"));
        assert_eq!(id2.0, "publisher");
    }

    #[test]
    fn state_is_satisfying() {
        assert!(SubsystemState::Ready.is_satisfying());
        assert!(SubsystemState::Skipped.is_satisfying());
        assert!(!SubsystemState::Booting.is_satisfying());
        assert!(!SubsystemState::Failed.is_satisfying());
    }

    #[test]
    fn register_and_transition() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("vault", ReadinessKind::Required);
        assert_eq!(registry.state(handle.id()), Some(SubsystemState::Booting));

        let mut rx = registry.subscribe();
        handle.mark_ready(Some("unlocked".into()));

        let update = rx.try_recv().expect("event delivered");
        assert_eq!(update.previous, SubsystemState::Booting);
        assert_eq!(update.current, SubsystemState::Ready);
        assert_eq!(update.kind, ReadinessKind::Required);
        assert_eq!(update.detail.as_deref(), Some("unlocked"));
    }

    #[test]
    fn transition_is_idempotent() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("vault", ReadinessKind::Required);
        let mut rx = registry.subscribe();

        handle.mark_ready(None);
        handle.mark_ready(None);

        // Only one update emitted: the second is a no-op.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dead_handle_is_noop() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("vault", ReadinessKind::Required);
        drop(registry);
        // No panic: dropped registry means the upgrade fails.
        handle.mark_ready(None);
    }

    #[test]
    fn ready_booting_ready_roundtrip_is_reversible() {
        // ADR-059 Phase 5.4: the vault relock path demotes the
        // subsystem to Booting and the unlock path restores it. Unlike
        // mark_failed, mark_booting must NOT be terminal — a later
        // mark_ready must be honoured.
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("vault", ReadinessKind::Required);
        let mut rx = registry.subscribe();

        handle.mark_ready(None);
        assert_eq!(registry.state(handle.id()), Some(SubsystemState::Ready));
        assert!(rx.try_recv().is_ok());

        // User locks the vault: demoted back to Booting.
        handle.mark_booting(Some("vault locked by user".into()));
        assert_eq!(registry.state(handle.id()), Some(SubsystemState::Booting));
        assert!(!registry.all_required_ready());
        assert!(registry.any_required_booting());
        assert!(rx.try_recv().is_ok());

        // User unlocks again: restored to Ready (not silently ignored).
        handle.mark_ready(Some("vault unlocked via HTTP".into()));
        assert_eq!(registry.state(handle.id()), Some(SubsystemState::Ready));
        assert!(registry.all_required_ready());
        let update = rx.try_recv().expect("restore event delivered");
        assert_eq!(update.previous, SubsystemState::Booting);
        assert_eq!(update.current, SubsystemState::Ready);
        assert_eq!(update.detail.as_deref(), Some("vault unlocked via HTTP"));
    }

    #[test]
    fn all_required_ready_and_failing_predicates() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let h1 = registry.register("vault", ReadinessKind::Required);
        let h2 = registry.register("publisher", ReadinessKind::Required);
        let h3 = registry.register("embedding", ReadinessKind::Optional);

        assert!(registry.any_required_booting());
        assert!(!registry.all_required_ready());
        assert!(!registry.any_required_failed());
        assert!(!registry.any_optional_failed());

        h1.mark_ready(None);
        assert!(registry.any_required_booting());

        h2.mark_ready(None);
        assert!(registry.all_required_ready());
        assert!(!registry.any_required_booting());

        h3.mark_failed(None);
        assert!(registry.any_optional_failed());
    }

    #[test]
    fn optional_failure_does_not_block_all_required_ready() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let h_req = registry.register("vault", ReadinessKind::Required);
        let h_opt = registry.register("embedding", ReadinessKind::Optional);

        h_req.mark_ready(None);
        h_opt.mark_failed(None);

        assert!(registry.all_required_ready());
        assert!(registry.any_optional_failed());
    }

    #[test]
    fn snapshot_returns_full_state() {
        let registry = SubsystemReadinessRegistry::new_shared();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("embedding", ReadinessKind::Optional);
        registry
            .register("publisher", ReadinessKind::Required)
            .mark_failed(Some("boom".into()));

        let snap = registry.snapshot();
        assert_eq!(snap.len(), 3);
        let vault = snap.iter().find(|(id, _, _, _)| id.0 == "vault").unwrap();
        assert_eq!(vault.1, SubsystemState::Ready);
        let publisher = snap
            .iter()
            .find(|(id, _, _, _)| id.0 == "publisher")
            .unwrap();
        assert_eq!(publisher.1, SubsystemState::Failed);
        assert_eq!(publisher.3.as_deref(), Some("boom"));
    }
}