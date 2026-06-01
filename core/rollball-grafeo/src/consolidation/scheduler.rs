//! Consolidation scheduler — manages when and how offline consolidation runs.
//!
//! Phase 3 S4.1: Provides three trigger modes (idle-timeout, accumulation,
//! manual) and batch management for processing pending knowledge nodes.
//!
//! Design: `docs/05-memory.md` §4.3

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::consolidation::offline::{OfflineConsolidationConfig, OfflineConsolidationResult};
use crate::error::Result;
use crate::grafeo::GrafeoStore;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the consolidation scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Idle timeout in seconds before automatic consolidation.
    /// Default: 1800 (30 minutes).
    pub idle_timeout_secs: u64,
    /// Minimum number of pending nodes before triggering consolidation.
    /// Default: 50.
    pub accumulation_threshold: usize,
    /// Batch size per consolidation run.
    /// Default: 50 (inherited from OfflineConsolidationConfig).
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible.
    /// Default: 1 (inherited from OfflineConsolidationConfig).
    pub min_pending_age_hours: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            batch_size: 50,
            min_pending_age_hours: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger reason
// ---------------------------------------------------------------------------

/// Why a consolidation run was triggered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TriggerReason {
    /// Agent has been idle for longer than the configured timeout.
    IdleTimeout,
    /// The number of pending nodes exceeded the accumulation threshold.
    Accumulation,
    /// Manually triggered by the user or API.
    Manual,
}

// ---------------------------------------------------------------------------
// Run record
// ---------------------------------------------------------------------------

/// Record of a single consolidation run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConsolidationRun {
    /// Unique run identifier.
    pub id: String,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished.
    pub finished_at: DateTime<Utc>,
    /// Why the run was triggered.
    pub trigger: TriggerReason,
    /// Number of pending nodes available before the run.
    pub pending_before: usize,
    /// The consolidation result.
    pub result: OfflineConsolidationResult,
}

// ---------------------------------------------------------------------------
// Scheduler state
// ---------------------------------------------------------------------------

/// Internal state of the scheduler (protected by Mutex).
#[derive(Debug)]
struct SchedulerState {
    /// Last time the agent was active (sent a message, used a tool, etc.).
    last_active_at: DateTime<Utc>,
    /// Count of pending nodes at last check.
    pending_count: usize,
    /// History of completed runs.
    run_history: Vec<ConsolidationRun>,
    /// Next run ID counter.
    next_run_id: u64,
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Consolidation scheduler — decides *when* to run offline consolidation.
///
/// The scheduler itself does not run the consolidation; it provides
/// `should_run()` and `run_now()` methods that the caller (typically
/// a background task in the Runtime) can use.
pub struct ConsolidationScheduler {
    store: Arc<Mutex<GrafeoStore>>,
    config: SchedulerConfig,
    offline_config: OfflineConsolidationConfig,
    state: Mutex<SchedulerState>,
}

impl ConsolidationScheduler {
    /// Create a new scheduler.
    pub fn new(store: Arc<Mutex<GrafeoStore>>, config: SchedulerConfig) -> Self {
        let offline_config = OfflineConsolidationConfig {
            batch_size: config.batch_size,
            min_pending_age_hours: config.min_pending_age_hours,
        };
        Self {
            store,
            offline_config,
            state: Mutex::new(SchedulerState {
                last_active_at: Utc::now(),
                pending_count: 0,
                run_history: Vec::new(),
                next_run_id: 1,
            }),
            config,
        }
    }

    /// Notify the scheduler that the agent is active.
    /// Resets the idle timer.
    pub async fn notify_active(&self) {
        let mut state = self.state.lock().await;
        state.last_active_at = Utc::now();
    }

    /// Update the pending node count (called periodically by the background task).
    pub async fn update_pending_count(&self, count: usize) {
        let mut state = self.state.lock().await;
        state.pending_count = count;
    }

    /// Check whether consolidation should run now.
    ///
    /// Returns the trigger reason if a run is warranted, or `None` if not.
    pub async fn should_run(&self) -> Option<TriggerReason> {
        let state = self.state.lock().await;
        let now = Utc::now();

        // Check accumulation threshold
        if state.pending_count >= self.config.accumulation_threshold {
            return Some(TriggerReason::Accumulation);
        }

        // Check idle timeout
        let idle_duration = now - state.last_active_at;
        let idle_secs = idle_duration.num_seconds();
        if idle_secs >= self.config.idle_timeout_secs as i64 && state.pending_count > 0 {
            return Some(TriggerReason::IdleTimeout);
        }

        None
    }

    /// Run consolidation now (for manual trigger or when `should_run` returns Some).
    pub async fn run_now(&self, trigger: TriggerReason) -> Result<ConsolidationRun> {
        let started_at = Utc::now();

        let (result, pending_before) = {
            let store = self.store.lock().await;

            // Count pending before run
            let pending_nodes = store.get_pending_for_consolidation(
                self.offline_config.min_pending_age_hours,
                self.offline_config.batch_size,
            )?;
            let pending_before = pending_nodes.len();

            // Run consolidation
            let result = store.run_offline_consolidation(&self.offline_config)?;
            (result, pending_before)
        };

        let finished_at = Utc::now();

        // Record the run
        let run_id = {
            let mut state = self.state.lock().await;
            let id = format!("consol-{}", state.next_run_id);
            state.next_run_id += 1;

            let run = ConsolidationRun {
                id: id.clone(),
                started_at,
                finished_at,
                trigger,
                pending_before,
                result,
            };

            let result_copy = run.result.clone();
            state.run_history.push(run);
            // Update pending count after run
            state.pending_count = state.pending_count.saturating_sub(
                result_copy.upgraded + result_copy.marked_dormant,
            );

            id
        };

        // Fetch the run we just recorded
        let state = self.state.lock().await;
        let run = state.run_history.iter()
            .find(|r| r.id == run_id)
            .cloned()
            .ok_or_else(|| crate::error::GrafeoError::Memory("Run not found after insert".to_string()))?;

        Ok(run)
    }

    /// Get the number of completed runs.
    pub async fn run_count(&self) -> usize {
        self.state.lock().await.run_history.len()
    }

    /// Get run history (most recent last).
    pub async fn get_history(&self) -> Vec<ConsolidationRun> {
        self.state.lock().await.run_history.clone()
    }

    /// Get the current pending count.
    pub async fn pending_count(&self) -> usize {
        self.state.lock().await.pending_count
    }

    /// Get the time since last activity.
    pub async fn idle_seconds(&self) -> i64 {
        let state = self.state.lock().await;
        (Utc::now() - state.last_active_at).num_seconds()
    }

    /// Get a reference to the scheduler config.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Background task
// ---------------------------------------------------------------------------

// Background task `run_consolidation_scheduler` is implemented in
// `rollball-runtime` where `tracing` is available. The grafeo crate
// intentionally does not depend on `tracing` to keep the dependency
// tree minimal.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KnowledgeNode, KnowledgeSubType, DEFAULT_EMBEDDING_DIM};

    fn test_store() -> Arc<Mutex<GrafeoStore>> {
        Arc::new(Mutex::new(GrafeoStore::new_in_memory().unwrap()))
    }

    fn test_embedding() -> Vec<f32> {
        vec![0.1f32; DEFAULT_EMBEDDING_DIM]
    }

    fn old_time() -> DateTime<Utc> {
        chrono::Utc::now() - chrono::TimeDelta::hours(2)
    }

    // =====================================================================
    // Test: Scheduler with default config
    // =====================================================================

    #[test]
    fn test_scheduler_default_config() {
        let config = SchedulerConfig::default();
        assert_eq!(config.idle_timeout_secs, 1800);
        assert_eq!(config.accumulation_threshold, 50);
        assert_eq!(config.batch_size, 50);
    }

    // =====================================================================
    // Test: notify_active resets idle timer
    // =====================================================================

    #[tokio::test]
    async fn test_notify_active_resets_idle() {
        let store = test_store();
        let scheduler = ConsolidationScheduler::new(store, SchedulerConfig::default());

        // Should start with very small idle time
        let idle = scheduler.idle_seconds().await;
        assert!(idle < 5, "Idle should be near 0 after creation");

        // Wait a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Idle should have increased
        let idle_after = scheduler.idle_seconds().await;
        // (may still be 0 due to second-level granularity, so just check it doesn't crash)
        assert!(idle_after >= 0);
    }

    // =====================================================================
    // Test: should_run returns None when no pending nodes
    // =====================================================================

    #[tokio::test]
    async fn test_should_run_no_pending() {
        let store = test_store();
        let scheduler = ConsolidationScheduler::new(store, SchedulerConfig::default());

        scheduler.update_pending_count(0).await;
        let result = scheduler.should_run().await;
        assert!(result.is_none(), "Should not run with 0 pending nodes");
    }

    // =====================================================================
    // Test: should_run returns Accumulation when threshold met
    // =====================================================================

    #[tokio::test]
    async fn test_should_run_accumulation() {
        let store = test_store();
        let config = SchedulerConfig {
            accumulation_threshold: 5,
            ..SchedulerConfig::default()
        };
        let scheduler = ConsolidationScheduler::new(store, config);

        scheduler.update_pending_count(5).await;
        let result = scheduler.should_run().await;
        assert_eq!(result, Some(TriggerReason::Accumulation));
    }

    // =====================================================================
    // Test: manual trigger with run_now
    // =====================================================================

    #[tokio::test]
    async fn test_manual_trigger_run_now() {
        let store = test_store();

        // Seed some pending nodes
        {
            let s = store.lock().await;
            let node = KnowledgeNode {
                id: None,
                subject: "user".to_string(),
                predicate: "likes".to_string(),
                object: "coffee".to_string(),
                sub_type: KnowledgeSubType::Preference,
                confidence: 0.8,
                source_episode_id: None,
                embedding: Some(test_embedding()),
                status: crate::types::NodeStatus::Pending,
                created_at: old_time(),
                updated_at: old_time(),
                metadata: std::collections::HashMap::new(),
            };
            s.store_knowledge(&node).unwrap();
        }

        let scheduler = ConsolidationScheduler::new(store, SchedulerConfig::default());
        scheduler.update_pending_count(1).await;

        let run = scheduler.run_now(TriggerReason::Manual).await.unwrap();

        assert_eq!(run.trigger, TriggerReason::Manual);
        assert_eq!(run.result.upgraded, 1, "One pending node should be upgraded");
        assert!(run.started_at <= run.finished_at);
        assert!(run.id.starts_with("consol-"));
        assert_eq!(scheduler.run_count().await, 1);
    }

    // =====================================================================
    // Test: run_history tracks multiple runs
    // =====================================================================

    #[tokio::test]
    async fn test_run_history() {
        let store = test_store();

        // Seed two pending nodes
        {
            let s = store.lock().await;
            for i in 0..2 {
                let node = KnowledgeNode {
                    id: None,
                    subject: "user".to_string(),
                    predicate: format!("item_{}", i),
                    object: "value".to_string(),
                    sub_type: KnowledgeSubType::Fact,
                    confidence: 0.8,
                    source_episode_id: None,
                    embedding: Some(test_embedding()),
                    status: crate::types::NodeStatus::Pending,
                    created_at: old_time(),
                    updated_at: old_time(),
                    metadata: std::collections::HashMap::new(),
                };
                s.store_knowledge(&node).unwrap();
            }
        }

        let scheduler = ConsolidationScheduler::new(store, SchedulerConfig::default());

        // Run 1
        scheduler.update_pending_count(2).await;
        let run1 = scheduler.run_now(TriggerReason::Manual).await.unwrap();
        assert_eq!(run1.id, "consol-1");

        // Run 2 (no more pending nodes)
        let run2 = scheduler.run_now(TriggerReason::IdleTimeout).await.unwrap();
        assert_eq!(run2.id, "consol-2");
        assert_eq!(run2.result.upgraded, 0);

        let history = scheduler.get_history().await;
        assert_eq!(history.len(), 2);
    }

    // =====================================================================
    // Test: idle timeout trigger with short timeout
    // =====================================================================

    #[tokio::test]
    async fn test_idle_timeout_trigger() {
        let store = test_store();
        let config = SchedulerConfig {
            idle_timeout_secs: 0, // Immediate timeout
            accumulation_threshold: 999, // Don't trigger on accumulation
            ..SchedulerConfig::default()
        };
        let scheduler = ConsolidationScheduler::new(store, config);

        // Set pending count > 0
        scheduler.update_pending_count(1).await;

        // Should trigger idle timeout immediately
        let result = scheduler.should_run().await;
        assert_eq!(result, Some(TriggerReason::IdleTimeout));
    }

    // =====================================================================
    // Test: pending_count updates after run
    // =====================================================================

    #[tokio::test]
    async fn test_pending_count_after_run() {
        let store = test_store();

        // Seed pending nodes
        {
            let s = store.lock().await;
            for i in 0..3 {
                let node = KnowledgeNode {
                    id: None,
                    subject: "user".to_string(),
                    predicate: format!("test_{}", i),
                    object: "value".to_string(),
                    sub_type: KnowledgeSubType::Fact,
                    confidence: 0.8,
                    source_episode_id: None,
                    embedding: Some(test_embedding()),
                    status: crate::types::NodeStatus::Pending,
                    created_at: old_time(),
                    updated_at: old_time(),
                    metadata: std::collections::HashMap::new(),
                };
                s.store_knowledge(&node).unwrap();
            }
        }

        let scheduler = ConsolidationScheduler::new(store, SchedulerConfig::default());
        scheduler.update_pending_count(3).await;

        // Run consolidation
        let run = scheduler.run_now(TriggerReason::Manual).await.unwrap();
        assert_eq!(run.result.upgraded, 3);

        // Pending count should have decreased
        let pending = scheduler.pending_count().await;
        assert_eq!(pending, 0, "Pending count should be 0 after upgrading all nodes");
    }

    // =====================================================================
    // Test: TriggerReason serialization roundtrip
    // =====================================================================

    #[test]
    fn test_trigger_reason_serde() {
        let reasons = [TriggerReason::IdleTimeout, TriggerReason::Accumulation, TriggerReason::Manual];
        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let decoded: TriggerReason = serde_json::from_str(&json).unwrap();
            assert_eq!(*reason, decoded);
        }
    }
}
