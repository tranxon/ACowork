//! ADR-059 §6.2 — in-memory operation store.
//!
//! Tracks every accepted mutation operation (`OperationRecord`) keyed
//! by `OperationId`. The store is intentionally in-memory: operations
//! are short-lived (default 60 s deadline) and the Gateway is the only
//! process that transitions them. Persistence is a future extension —
//! the store API is a plain map so a durable backend can be swapped in
//! without touching callers.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::operation::{OperationId, OperationRecord, OperationState};
use chrono::Utc;
use parking_lot::Mutex;

/// Shared handle to the operation store.
pub type SharedOperationStore = Arc<OperationStore>;

/// In-memory `operation_id → record` map with O(1) idempotency checks.
pub struct OperationStore {
    inner: Mutex<HashMap<OperationId, OperationRecord>>,
}

impl OperationStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Create a shared store.
    pub fn new_shared() -> SharedOperationStore {
        Arc::new(Self::new())
    }

    /// Insert a fresh record. Idempotency guarantee: if `operation_id`
    /// already exists the record is NOT overwritten and `false` is
    /// returned (duplicate submit detection, ADR-059 §7.4).
    pub fn insert(&self, record: OperationRecord) -> bool {
        self.inner.lock().insert(record.operation_id.clone(), record).is_none()
    }

    /// Look up a record by id.
    pub fn get(&self, operation_id: &OperationId) -> Option<OperationRecord> {
        self.inner.lock().get(operation_id).cloned()
    }

    /// Look up a record by its UUID string form.
    pub fn get_by_str(&self, operation_id: &str) -> Option<OperationRecord> {
        let key = OperationId::from(operation_id.to_string());
        self.get(&key)
    }

    /// Transition a record to a new state. Returns `None` when the
    /// operation is unknown; `Some(old_record)` otherwise. Terminal
    /// states are sticky: a `Completed`/`Failed` record ignores further
    /// transitions (a late NodeEvent must not flip a completed
    /// operation back to Running).
    pub fn transition(
        &self,
        operation_id: &OperationId,
        state: OperationState,
        resource_version: Option<u64>,
        terminal_error: Option<acowork_core::error_codes::StructuredErrorBody>,
    ) -> Option<OperationRecord> {
        let mut inner = self.inner.lock();
        let record = inner.get_mut(operation_id)?;
        if record.state.is_terminal() {
            return Some(record.clone());
        }
        record.state = state;
        if resource_version.is_some() {
            record.resource_version = resource_version;
        }
        if terminal_error.is_some() {
            record.terminal_error = terminal_error;
        }
        Some(record.clone())
    }

    /// Mark an operation failed with a structured error (idempotent —
    /// no-op on unknown ids and on already-terminal records).
    pub fn fail(
        &self,
        operation_id: &OperationId,
        error: acowork_core::error_codes::StructuredErrorBody,
    ) {
        let _ = self.transition(operation_id, OperationState::Failed, None, Some(error));
    }

    /// Remove a record (completed operations are swept by callers).
    pub fn remove(&self, operation_id: &OperationId) -> Option<OperationRecord> {
        self.inner.lock().remove(operation_id)
    }

    /// Number of tracked operations (diagnostics / tests).
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the store tracks no operations.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sweep expired, non-terminal records. Returns the number removed.
    pub fn sweep_expired(&self) -> usize {
        let now = Utc::now();
        let mut inner = self.inner.lock();
        let before = inner.len();
        inner.retain(|_, r| !r.is_expired(now));
        before - inner.len()
    }
}

impl Default for OperationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::error_codes::StructuredErrorBody;

    #[test]
    fn insert_detects_duplicate_operation_id() {
        let store = OperationStore::new();
        let record = OperationRecord::new(0);
        assert!(store.insert(record.clone()));
        // Same id submitted twice → rejected (idempotency).
        assert!(!store.insert(record.clone()));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn transition_updates_state_and_version() {
        let store = OperationStore::new();
        let record = OperationRecord::new(0);
        let id = record.operation_id.clone();
        store.insert(record);

        let updated = store
            .transition(&id, OperationState::Committed, Some(7), None)
            .expect("record exists");
        assert_eq!(updated.state, OperationState::Committed);
        assert_eq!(updated.resource_version, Some(7));
    }

    #[test]
    fn terminal_state_is_sticky() {
        let store = OperationStore::new();
        let record = OperationRecord::new(0);
        let id = record.operation_id.clone();
        store.insert(record);

        store.transition(&id, OperationState::Completed, Some(7), None);
        // A late event must not un-complete the operation.
        let after = store.transition(&id, OperationState::Running, None, None).unwrap();
        assert_eq!(after.state, OperationState::Completed);
    }

    #[test]
    fn fail_sets_terminal_error() {
        let store = OperationStore::new();
        let record = OperationRecord::new(0);
        let id = record.operation_id.clone();
        store.insert(record);

        let error = StructuredErrorBody::dependency_not_ready(
            Some("BOOTING".to_string()),
            None,
            500,
        );
        store.fail(&id, error);
        let record = store.get(&id).unwrap();
        assert_eq!(record.state, OperationState::Failed);
        assert!(record.terminal_error.is_some());
    }

    #[test]
    fn unknown_operation_transition_is_noop() {
        let store = OperationStore::new();
        let id = OperationId::new();
        assert!(store.transition(&id, OperationState::Completed, None, None).is_none());
        store.fail(&id, StructuredErrorBody::dependency_not_ready(None, None, 0));
        assert!(store.is_empty());
    }

    #[test]
    fn sweep_expired_removes_stale_operations() {
        let store = OperationStore::new();
        let mut record = OperationRecord::new(0);
        // Age the record past its deadline.
        record.deadline_at = Utc::now() - chrono::Duration::seconds(1);
        let id = record.operation_id.clone();
        store.insert(record);
        assert_eq!(store.sweep_expired(), 1);
        assert!(store.get(&id).is_none());
    }
}
