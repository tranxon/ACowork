//! request_id de-duplication for node control commands (ADR-055 §6.2).
//!
//! MQTT QoS 1 is at-least-once: duplicates are the norm, not the
//! exception. Every incoming `NodeControlCommand` passes through a
//! [`RequestDedup`] before execution:
//!
//! - unseen `request_id` → register → execute → cache the reply;
//! - seen `request_id`   → do NOT re-execute; re-send the cached
//!   reply verbatim.
//!
//! This is the first half of the idempotency discipline; the second
//! half (command-level idempotent semantics: `start` on a running
//! agent succeeds with the existing PID, etc.) lands with the
//! process/package modules in Phase 2b.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use acowork_core::mqtt_proto::NodeEvent;

/// Default capacity of the de-duplication cache. 256 recent requests
/// comfortably covers a reconnect-storm replay window while bounding
/// memory.
const DEFAULT_CAPACITY: usize = 256;

/// Default TTL of cached replies. Entries older than this are evicted;
/// after the TTL a re-delivery is treated as a fresh request (safe
/// because Phase 2b+ commands are individually idempotent).
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// Bounded reply cache keyed by request_id.
///
/// Not a true LRU (no access-order promotion) — a bounded FIFO
/// eviction is sufficient: what matters is covering the QoS 1
/// re-delivery window, not caching the hottest ids.
pub struct RequestDedup {
    capacity: usize,
    ttl: Duration,
    entries: HashMap<String, (Instant, NodeEvent)>,
    order: std::collections::VecDeque<String>,
}

impl Default for RequestDedup {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL)
    }
}

impl RequestDedup {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    /// Check whether `request_id` was already handled. Stale entries
    /// (past the TTL) are evicted and the id is treated as fresh.
    pub fn contains(&mut self, request_id: &str) -> bool {
        match self.entries.get(request_id) {
            Some((at, _)) => {
                if at.elapsed() < self.ttl {
                    true
                } else {
                    self.entries.remove(request_id);
                    false
                }
            }
            None => false,
        }
    }

    /// Fetch the cached reply for a previously seen request_id.
    /// Returns `None` for unseen or evicted ids.
    pub fn cached_reply(&self, request_id: &str) -> Option<&NodeEvent> {
        self.entries.get(request_id).map(|(_, ev)| ev)
    }

    /// Cache the reply for `request_id`, evicting the oldest entry
    /// when at capacity.
    pub fn insert(&mut self, request_id: &str, reply: NodeEvent) {
        if !self.entries.contains_key(request_id) {
            self.order.push_back(request_id.to_string());
            while self.order.len() > self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries
            .insert(request_id.to_string(), (Instant::now(), reply));
    }

    /// Number of cached entries (diagnostics / tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(request_id: &str, message: &str) -> NodeEvent {
        NodeEvent {
            node_id: "local".to_string(),
            request_id: request_id.to_string(),
            status: "ok".to_string(),
            message: message.to_string(),
            result_json: None,
        }
    }

    #[test]
    fn first_request_is_unseen() {
        let mut dedup = RequestDedup::default();
        assert!(!dedup.contains("req-1"));
        assert_eq!(dedup.cached_reply("req-1"), None);
    }

    #[test]
    fn duplicate_returns_cached_reply() {
        let mut dedup = RequestDedup::default();
        dedup.insert("req-1", event("req-1", "first"));
        assert!(dedup.contains("req-1"));
        let cached = dedup.cached_reply("req-1").unwrap();
        assert_eq!(cached.message, "first");

        // A duplicate insert (re-execution guard violation in caller)
        // overwrites the cache; contains stays true.
        dedup.insert("req-1", event("req-1", "second"));
        assert_eq!(dedup.cached_reply("req-1").unwrap().message, "second");
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut dedup = RequestDedup::new(2, DEFAULT_TTL);
        dedup.insert("req-1", event("req-1", "1"));
        dedup.insert("req-2", event("req-2", "2"));
        dedup.insert("req-3", event("req-3", "3"));

        assert!(!dedup.contains("req-1"));
        assert!(dedup.contains("req-2"));
        assert!(dedup.contains("req-3"));
        assert_eq!(dedup.len(), 2);
    }

    #[test]
    fn ttl_expiry_treats_request_as_fresh() {
        let mut dedup = RequestDedup::new(8, Duration::from_millis(0));
        dedup.insert("req-1", event("req-1", "old"));
        // TTL 0 — the entry is instantly stale: contains() must report
        // false and the cached reply must be gone.
        assert!(!dedup.contains("req-1"));
        assert_eq!(dedup.cached_reply("req-1"), None);
        assert!(dedup.is_empty());
    }
}
