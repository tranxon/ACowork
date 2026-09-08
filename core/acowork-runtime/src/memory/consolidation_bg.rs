//! Consolidation background task - timing policy and background loop.
//!
//! ADR-051 P4: Replaces the grafeo `ConsolidationScheduler` with a
//! lightweight `ConsolidationTimer` that lives in the Runtime. The timer
//! implements interval-gated triggers (distiller + episodic forgetting)
//! without needing a GrafeoStore. The legacy Pending-count accumulation /
//! idle-timeout triggers are gone — Pending nodes have had no producer
//! since ADR-068, so those triggers could never fire.
//!
//! The background task:
//! 1. Polls `should_run_distill()` / `should_run_forgetting()` every poll
//!    interval (default 60s)
//! 2. When triggered, runs the EpisodicDistiller and/or the episodic
//!    forgetting decay scan
//! 3. Logs results and errors
//!
//! The actual consolidation execution goes through `dyn MemoryProvider`,
//! so any provider backend can be used.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use acowork_memory::consolidation::{
    SchedulerConfig, TripleExtractorLlm,
};
use acowork_memory::EpisodicDecayConfig;
use chrono::Utc;
use tokio::sync::Mutex;

use crate::embedding::EmbeddingProvider;
use crate::memory::llm_adapter::ProviderLlmAdapter;

// ---------------------------------------------------------------------------
// Trigger reason
// ---------------------------------------------------------------------------

/// Why a consolidation run was triggered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerReason {
    /// Episodic forgetting scan interval point reached (opt-in).
    ForgettingInterval,
    /// Distiller (ADR-071 D1): interval point reached and the
    /// unconsolidated-episode backlog is at/above the accumulation threshold.
    DistillerAccumulation,
    /// Distiller (ADR-071 D1): interval point reached, the backlog is
    /// non-empty and the agent has been idle long enough.
    DistillerIdle,
}

// ---------------------------------------------------------------------------
// Consolidation timer (replaces grafeo's ConsolidationScheduler)
// ---------------------------------------------------------------------------

/// Lightweight scheduling policy for consolidation runs.
///
/// ADR-051 P4: Replaces `acowork_grafeo::consolidation::ConsolidationScheduler`.
/// Does NOT hold a store reference - the background loop calls
/// `dyn MemoryProvider` for all data operations.
pub struct ConsolidationTimer {
    /// Scheduler policy. Wrapped in `RwLock` so a live config change
    /// (ADR-071 D6 — memory-panel PUT) can swap the distiller switch /
    /// interval / thresholds without tearing down the background task.
    /// The loop re-reads `config()` every tick (60s), so the new policy
    /// takes effect on the next poll at the latest.
    config: RwLock<SchedulerConfig>,
    state: Mutex<TimerState>,
}

/// Summary of the most recent EpisodicDistiller run (ADR-071 D2). Stored on
/// the timer so `GET /memory/consolidation/status` can surface "上次运行"
/// without re-scanning the store.
#[derive(Debug, Clone, Default)]
pub struct DistillRunRecord {
    pub at: chrono::DateTime<Utc>,
    pub episodes_scanned: usize,
    pub facts_promoted: usize,
    pub preferences_promoted: usize,
    pub relations_promoted: usize,
    pub procedures_promoted: usize,
    pub autobio_promoted: usize,
    pub episodes_marked_consolidated: usize,
}

impl DistillRunRecord {
    /// Build a record from a distilled-run result. `at` defaults to now.
    pub fn from_result(
        at: chrono::DateTime<Utc>,
        result: &acowork_memory::consolidation::DistillerResult,
    ) -> Self {
        Self {
            at,
            episodes_scanned: result.episodes_scanned,
            facts_promoted: result.facts_promoted,
            preferences_promoted: result.preferences_promoted,
            relations_promoted: result.relations_promoted,
            procedures_promoted: result.procedures_promoted,
            autobio_promoted: result.autobio_promoted,
            episodes_marked_consolidated: result.episodes_marked_consolidated,
        }
    }

    /// Total promoted nodes across all categories.
    pub fn total_promoted(&self) -> usize {
        self.facts_promoted
            + self.preferences_promoted
            + self.relations_promoted
            + self.procedures_promoted
            + self.autobio_promoted
    }
}

#[derive(Debug)]
struct TimerState {
    last_active_at: chrono::DateTime<Utc>,
    /// Unconsolidated-episode backlog (distiller input, ADR-071 D1).
    episode_count: usize,
    /// Last time the distiller actually ran (interval gate, ADR-071 D1).
    last_distill_at: chrono::DateTime<Utc>,
    /// Last time the episodic forgetting scan ran (interval gate).
    last_forgetting_at: chrono::DateTime<Utc>,
    /// Summary of the most recent distiller run (ADR-071 D2). `None` until
    /// the first run completes.
    last_distill: Option<DistillRunRecord>,
}

impl ConsolidationTimer {
    pub fn new(config: SchedulerConfig) -> Self {
        let now = Utc::now();
        Self {
            config: RwLock::new(config),
            state: Mutex::new(TimerState {
                last_active_at: now,
                episode_count: 0,
                last_distill_at: now,
                last_forgetting_at: now,
                last_distill: None,
            }),
        }
    }

    /// Notify the timer that the agent is active. Resets the idle timer.
    pub async fn notify_active(&self) {
        let mut state = self.state.lock().await;
        state.last_active_at = Utc::now();
    }

    /// Update the unconsolidated-episode backlog (distiller input, ADR-071 D1).
    pub async fn update_episode_count(&self, count: usize) {
        let mut state = self.state.lock().await;
        state.episode_count = count;
    }

    /// Record that a distiller run just happened (ADR-071 D1 interval gate).
    pub async fn mark_distill_run(&self) {
        let mut state = self.state.lock().await;
        state.last_distill_at = Utc::now();
    }

    /// Record that a distiller run just happened, together with its result
    /// summary (ADR-071 D2). Resets the interval gate and stores the summary
    /// for the status endpoint.
    pub async fn record_distill_result(&self, result: &DistillRunRecord) {
        let mut state = self.state.lock().await;
        state.last_distill_at = result.at;
        state.last_distill = Some(result.clone());
    }

    /// Get the most recent distiller-run summary, if any (ADR-071 D2).
    pub async fn last_distill_result(&self) -> Option<DistillRunRecord> {
        let state = self.state.lock().await;
        state.last_distill.clone()
    }

    /// Check whether the EpisodicDistiller should run (ADR-071 D1).
    ///
    /// Independent of [`Self::should_run`]: the distiller's trigger is
    /// decoupled from the legacy `Pending`-node count, which has had no
    /// producer since ADR-068 (memory_store writes episodes only; the
    /// distiller promotes straight to Active nodes). Instead the distiller
    /// fires at most once per `distiller_interval_secs`, provided the
    /// unconsolidated-episode backlog is at/above `distiller_accumulation`
    /// OR the agent has been idle for at least `distiller_idle_secs` with a
    /// non-empty backlog.
    pub async fn should_run_distill(&self) -> Option<TriggerReason> {
        let state = self.state.lock().await;
        let cfg = self.config.read().unwrap();
        if !cfg.distiller_enabled {
            return None;
        }
        let now = Utc::now();

        // Interval gate (outer): at most one run per configured period.
        let since_last = now - state.last_distill_at;
        if since_last.num_seconds() < cfg.distiller_interval_secs as i64 {
            return None;
        }

        // Inner gate: backlog accumulation or idle with a non-empty backlog.
        if state.episode_count >= cfg.distiller_accumulation {
            return Some(TriggerReason::DistillerAccumulation);
        }
        let idle_secs = (now - state.last_active_at).num_seconds();
        if idle_secs >= cfg.distiller_idle_secs as i64 && state.episode_count > 0 {
            return Some(TriggerReason::DistillerIdle);
        }

        None
    }

    /// Check whether the episodic forgetting scan should run.
    ///
    /// Opt-in (`forgetting_enabled`, default off) + interval gate
    /// (`forgetting_interval_secs`, default 1h) so an enabled scan never
    /// hits the full Episodic table on every poll tick.
    pub async fn should_run_forgetting(&self) -> Option<TriggerReason> {
        let state = self.state.lock().await;
        let cfg = self.config.read().unwrap();
        if !cfg.forgetting_enabled {
            return None;
        }
        let since_last = Utc::now() - state.last_forgetting_at;
        if since_last.num_seconds() < cfg.forgetting_interval_secs as i64 {
            return None;
        }
        Some(TriggerReason::ForgettingInterval)
    }

    /// Record that the forgetting scan just ran (interval gate).
    pub async fn mark_forgetting_run(&self) {
        let mut state = self.state.lock().await;
        state.last_forgetting_at = Utc::now();
    }

    /// Snapshot the current scheduler config (ADR-071 D6: may differ from
    /// the config the timer was constructed with after a live update).
    pub fn config(&self) -> SchedulerConfig {
        self.config.read().unwrap().clone()
    }

    /// Swap the scheduler policy at runtime (ADR-071 D6). The background
    /// loop re-reads `config()` every tick, so the new distiller switch /
    /// interval / thresholds take effect on the next poll without tearing
    /// down the task. Idle/backlog state (last run, last active) is
    /// deliberately preserved across the swap.
    pub fn update_config(&self, new_config: SchedulerConfig) {
        tracing::info!(
            distiller_enabled = new_config.distiller_enabled,
            distiller_interval_secs = new_config.distiller_interval_secs,
            distiller_accumulation = new_config.distiller_accumulation,
            distiller_idle_secs = new_config.distiller_idle_secs,
            "ConsolidationTimer: runtime scheduler config updated (ADR-071 D6)"
        );
        *self.config.write().unwrap() = new_config;
    }

    /// Get the current idle duration in seconds (since last `notify_active`).
    /// Used by the HTTP status endpoint and tests.
    pub async fn idle_secs(&self) -> i64 {
        let state = self.state.lock().await;
        (Utc::now() - state.last_active_at).num_seconds()
    }

    /// Get the current unconsolidated-episode backlog (ADR-071 D1).
    /// Used by the HTTP status endpoint.
    pub async fn episode_count(&self) -> usize {
        let state = self.state.lock().await;
        state.episode_count
    }

    /// Get the time elapsed since the last distiller run, in seconds.
    /// Used by the HTTP status endpoint and tests.
    pub async fn secs_since_distill(&self) -> i64 {
        let state = self.state.lock().await;
        (Utc::now() - state.last_distill_at).num_seconds()
    }
}

// ---------------------------------------------------------------------------
// Background task handle
// ---------------------------------------------------------------------------

/// Handle for the background consolidation task.
///
/// Dropping this handle cancels the background task (via `JoinHandle::abort`).
#[derive(Debug)]
pub struct ConsolidationBgTask {
    join_handle: tokio::task::JoinHandle<()>,
}

impl ConsolidationBgTask {
    /// Spawn the background consolidation task.
    pub fn spawn(
        scheduler: Arc<ConsolidationTimer>,
        provider: Arc<dyn acowork_memory::MemoryProvider>,
        llm: Arc<dyn TripleExtractorLlm>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        poll_interval: Duration,
        work_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let join_handle = tokio::spawn(async move {
            run_consolidation_loop(
                scheduler,
                provider,
                llm,
                embedding_provider,
                poll_interval,
                work_dir,
            )
            .await;
        });

        Self { join_handle }
    }

    /// Abort the background task.
    pub fn abort(&self) {
        self.join_handle.abort();
    }
}

impl Drop for ConsolidationBgTask {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn run_consolidation_loop(
    scheduler: Arc<ConsolidationTimer>,
    provider: Arc<dyn acowork_memory::MemoryProvider>,
    llm: Arc<dyn TripleExtractorLlm>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    poll_interval: Duration,
    work_dir: Option<std::path::PathBuf>,
) {
    tracing::info!(
        poll_interval_secs = poll_interval.as_secs(),
        "Consolidation background task started"
    );

    let mut interval = tokio::time::interval(poll_interval);
    // First tick fires immediately - skip it so we don't consolidate on startup.
    interval.tick().await;

    loop {
        interval.tick().await;

        // Update the unconsolidated-episode backlog (distiller input).
        // Only polled when the distiller is enabled (off by default, so a
        // disabled distiller costs nothing).
        let distiller_enabled = scheduler.config().distiller_enabled;
        if distiller_enabled {
            let episode_count = match provider.count_unconsolidated_episodes() {
                Ok(count) => count,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to count unconsolidated episodes for distiller");
                    continue;
                }
            };
            scheduler.update_episode_count(episode_count).await;
        }

        // ADR-071 D1 + episodic forgetting: the distiller and the forgetting
        // scan trigger independently. A tick that satisfies either condition
        // runs its own pipeline; neither blocks the other. The legacy
        // Pending-count trigger had no producer since ADR-068 (memory_store
        // writes episodes only; the distiller promotes straight to Active
        // nodes) — it is gone.
        let forgetting_trigger = scheduler.should_run_forgetting().await;
        let distill_trigger = if distiller_enabled {
            scheduler.should_run_distill().await
        } else {
            None
        };
        if forgetting_trigger.is_none() && distill_trigger.is_none() {
            continue;
        }
        if let Some(reason) = &forgetting_trigger {
            tracing::info!(?reason, "Episodic forgetting scan triggered");
        }
        if let Some(reason) = &distill_trigger {
            let backlog = scheduler.episode_count().await;
            tracing::info!(?reason, backlog, "EpisodicDistiller triggered");
        }

        // Build embedding function from the embedding provider. The bridge is
        // `Option`: on non-multi-thread runtimes we degrade to exact-key
        // clustering instead of risking a block_on panic inside a tokio
        // worker (ADR-071 W2 — the distiller calls the bridge synchronously
        // from async code).
        let embedding_fn = build_embedding_bridge(embedding_provider.clone());

        // ADR-071 D1: EpisodicDistiller step (off-by-default), triggered by
        // its OWN condition — not by the legacy Pending-node trigger. The
        // distiller promotes classified episodes to the semantic layer.
        if distill_trigger.is_some() {
            let scheduler_cfg = scheduler.config();
            let result = run_episodic_distiller_step(
                provider.as_ref(),
                &*llm,
                embedding_fn.as_ref(),
                &scheduler_cfg,
            )
            .await;
            // ADR-071 D2: persist the run summary (time + promotion counts)
            // for the status endpoint, and reset the interval gate.
            if let Some(result) = result {
                let record = DistillRunRecord::from_result(Utc::now(), &result);
                scheduler.record_distill_result(&record).await;
            } else {
                scheduler.mark_distill_run().await;
            }
        }

        // Run episodic forgetting (pure time decay) through the provider
        // trait. Opt-in (`forgetting_enabled`); when disabled the scan is a
        // no-op and episodic nodes never age out.
        if forgetting_trigger.is_some() {
            let cfg = scheduler.config();
            let decay_config = EpisodicDecayConfig {
                enabled: true,
                half_life_days: cfg.forgetting_half_life_days,
                dormant_threshold: cfg.forgetting_dormant_threshold,
                archive_days: cfg.forgetting_archive_days,
            };
            match provider.run_episodic_decay_scan(&decay_config) {
                Ok(result) => {
                    tracing::info!(
                        to_dormant = result.to_dormant,
                        purged = result.purged,
                        "Episodic forgetting scan complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Episodic forgetting scan failed");
                }
            }
            scheduler.mark_forgetting_run().await;
        }

        // Notify the provider that consolidation just ran.
        provider.notify_consolidation_active().await;

        // Optional: write a sentinel file for debugging.
        if let Some(ref work_dir) = work_dir {
            let sentinel = work_dir.join(".consolidation_last_run");
            let _ = std::fs::write(&sentinel, Utc::now().to_rfc3339());
        }
    }
}

// ---------------------------------------------------------------------------
// EpisodicDistiller step (ADR-068 M4)
// ---------------------------------------------------------------------------

type DistillerEmbeddingFn = Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>;

/// Build an embedding bridge for the distiller / offline consolidation.
///
/// The bridge is invoked synchronously from async code (distiller clustering
/// is a sync call inside an async task). Calling `Handle::block_on` directly
/// inside a tokio worker thread panics ("Cannot start a runtime from within
/// a runtime"), so on the multi-thread runtime we hand the call to a blocking
/// worker via `block_in_place`.
///
/// Returns `None` when no safe bridge is possible (current-thread runtime or
/// no runtime context): callers then degrade to exact-key clustering, which
/// is the distiller's documented fallback (ADR-068 §3.4.2 Step 2b).
fn build_embedding_bridge(
    embedding_provider: Arc<dyn EmbeddingProvider>,
) -> Option<DistillerEmbeddingFn> {
    // No runtime context at all (e.g. a pure sync test): cannot await safely.
    let handle = tokio::runtime::Handle::try_current().ok()?;
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        tracing::debug!(
            flavor = ?handle.runtime_flavor(),
            "Embedding bridge unavailable on non-multi-thread runtime; distiller will use exact-key clustering"
        );
        return None;
    }
    Some(Arc::new(move |text: &str| -> Vec<f32> {
        let text_owned = text.to_string();
        match tokio::task::block_in_place(|| handle.block_on(embedding_provider.embed(&text_owned))) {
            Ok(vec) => vec,
            Err(e) => {
                tracing::warn!(error = %e, "Embedding failed during consolidation, using zero vector");
                vec![]
            }
        }
    }))
}

/// Run one EpisodicDistiller pass inside the background consolidation loop.
///
/// ADR-068 M4: the distiller is the ONLY producer of semantic-layer nodes
/// (R3). It scans unconsolidated episodes tagged with a `knowledge_subtype`
/// and promotes evidence-backed clusters to Knowledge/Procedural/
/// Autobiographical nodes. The step is off-by-default (`distiller_enabled`),
/// so this function is a no-op unless explicitly configured.
///
/// Returns `Some(result)` after a real grafeo-backed run; `None` when the
/// `grafeo-backend` feature is off or when the run failed. The background
/// loop ignores the return value; the manual HTTP trigger (`POST
/// /memory/distill`, ADR-071 D2) surfaces it to the caller.
///
/// The distiller lives in `acowork-grafeo`, which is an optional runtime
/// dependency behind the `grafeo-backend` feature. When that feature is
/// disabled the step degrades to a logged no-op.
async fn run_episodic_distiller_step(
    provider: &dyn acowork_memory::MemoryProvider,
    llm: &dyn TripleExtractorLlm,
    embedding_fn: Option<&DistillerEmbeddingFn>,
    config: &SchedulerConfig,
) -> Option<acowork_memory::consolidation::DistillerResult> {
    // When grafeo is not compiled in, nothing to do.
    #[cfg(not(feature = "grafeo-backend"))]
    {
        let _ = (provider, llm, embedding_fn, config);
        tracing::warn!(
            "EpisodicDistiller step requested but acowork-grafeo is not enabled \
             (feature 'grafeo-backend' is off); skipping"
        );
        None
    }

    #[cfg(feature = "grafeo-backend")]
    {
        use acowork_grafeo::consolidation::{DefaultEpisodicDistiller, EpisodicDistiller};

        let distiller = DefaultEpisodicDistiller;
        let distiller_config = config.distiller_config.clone().unwrap_or_default();
        let run_result = match distiller
            .run(provider, Some(llm), embedding_fn, &distiller_config)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    episodes_scanned = result.episodes_scanned,
                    facts_promoted = result.facts_promoted,
                    preferences_promoted = result.preferences_promoted,
                    relations_promoted = result.relations_promoted,
                    procedures_promoted = result.procedures_promoted,
                    autobio_promoted = result.autobio_promoted,
                    episodes_marked_consolidated = result.episodes_marked_consolidated,
                    evaluations = result.promotion_evaluations.len(),
                    "EpisodicDistiller run complete (ADR-068)"
                );
                // Full audit trail at debug level (one line per evaluation).
                for eval in &result.promotion_evaluations {
                    tracing::debug!(
                        kind = ?eval.promoted_kind,
                        decision = ?eval.decision,
                        confidence = eval.llm_confidence,
                        evidence_score = eval.evidence_score,
                        episodes = ?eval.source_episode_ids,
                        "distiller evaluation"
                    );
                }
                Some(result)
            }
            Err(e) => {
                tracing::warn!(error = %e, "EpisodicDistiller run failed (ADR-068)");
                None
            }
        };

        // ADR-068 M8: 30-day Relationship promotion. Relationship is a
        // runtime-observed autobiographical category with a single producer
        // (the distiller, rule-based) — the old offline
        // `auto_generate_relationship_nodes` step was removed. Idempotent;
        // returns None until the collaboration span reaches 30 days.
        match distiller.promote_autobio_relationship(provider).await {
            Ok(Some(eval)) => {
                tracing::info!(
                    kind = ?eval.promoted_kind,
                    node_id = eval.promoted_node_id,
                    span_days = ?eval.evidence_score,
                    "Relationship node promoted (ADR-068 M8)"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Relationship promotion failed (ADR-068 M8)");
            }
        }

        run_result
    }
}

// ---------------------------------------------------------------------------
// Pipeline starter
// ---------------------------------------------------------------------------

/// Parameters for [`start_consolidation_pipeline`].
pub struct ConsolidationParams {
    pub provider: Arc<dyn acowork_memory::MemoryProvider>,
    pub llm_provider: Arc<dyn acowork_core::providers::traits::Provider>,
    pub model: String,
    pub embedding_provider: Arc<dyn EmbeddingProvider>,
    pub scheduler_config: SchedulerConfig,
    pub poll_interval: Duration,
    pub work_dir: Option<std::path::PathBuf>,
}

/// Create and start the consolidation background pipeline.
///
/// Returns the timer (for `notify_active()` calls) and the
/// background task handle (to be stored in AgentCore).
///
/// ADR-051 P4: Uses `ConsolidationTimer` (Runtime-internal) instead of
/// grafeo's `ConsolidationScheduler`. No GrafeoStore dependency.
pub fn start_consolidation_pipeline(
    params: ConsolidationParams,
) -> (Arc<ConsolidationTimer>, ConsolidationBgTask) {
    let llm_adapter = Arc::new(ProviderLlmAdapter::new(params.llm_provider, params.model));

    let scheduler = Arc::new(ConsolidationTimer::new(params.scheduler_config));

    let bg_task = ConsolidationBgTask::spawn(
        scheduler.clone(),
        params.provider,
        llm_adapter,
        params.embedding_provider,
        params.poll_interval,
        params.work_dir,
    );

    (scheduler, bg_task)
}

/// Run a single EpisodicDistiller pass on demand (ADR-071 D2 manual
/// trigger). Shares the exact same execution path as the background loop
/// (`run_episodic_distiller_step`), so the manual endpoint and the periodic
/// scheduler behave identically.
///
/// Returns the distilled-run summary (`None` when the `grafeo-backend`
/// feature is off, so the caller can report "not available").
pub(crate) async fn run_episodic_distiller_step_once(
    provider: Arc<dyn acowork_memory::MemoryProvider>,
    llm: Arc<dyn TripleExtractorLlm>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    config: SchedulerConfig,
) -> Option<acowork_memory::consolidation::DistillerResult> {
    let embedding_fn = build_embedding_bridge(embedding_provider);
    run_episodic_distiller_step(provider.as_ref(), &*llm, embedding_fn.as_ref(), &config).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_memory::consolidation::DistillerConfig;

    #[tokio::test]
    async fn test_timer_notify_active_resets_idle() {
        let timer = ConsolidationTimer::new(SchedulerConfig::default());
        timer.notify_active().await;
        let idle = {
            let state = timer.state.lock().await;
            (Utc::now() - state.last_active_at).num_seconds()
        };
        assert!(idle < 5, "Idle should be near 0 after notify_active");
    }

    // Legacy `test_timer_accumulation_trigger` / `test_timer_no_trigger_when_empty`
    // (Pending-count triggers) were deleted with the `should_run()` /
    // `update_pending_count()` methods — Pending has no producer since
    // ADR-068 (ADR-057 §5.3 redesign).

    // ── ADR-071 D1: distiller trigger (independent of legacy Pending) ──

    #[tokio::test]
    async fn test_distiller_trigger_disabled_by_default() {
        // distiller_enabled = false (default) → never fires even with backlog.
        let timer = ConsolidationTimer::new(SchedulerConfig {
            distiller_accumulation: 1,
            ..Default::default()
        });
        timer.update_episode_count(10).await;
        assert_eq!(timer.should_run_distill().await, None);
    }

    #[tokio::test]
    async fn test_distiller_trigger_interval_gate() {
        // Interval gate: backlog high but the previous run was < 1h ago.
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_accumulation: 5,
            distiller_interval_secs: 3600,
            distiller_idle_secs: 1800,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        timer.update_episode_count(10).await;
        // Fresh timer: last_distill_at = now → interval not elapsed.
        assert_eq!(timer.should_run_distill().await, None);
        // Even after a mark, still gated.
        timer.mark_distill_run().await;
        assert_eq!(timer.should_run_distill().await, None);
    }

    #[tokio::test]
    async fn test_distiller_trigger_live_config_update_d6() {
        // ADR-071 D6: a runtime config swap (memory-panel PUT) must take
        // effect on the next tick without tearing down the task. Start with
        // the distiller DISABLED → nothing fires even with a full backlog.
        let config = SchedulerConfig {
            distiller_enabled: false,
            distiller_accumulation: 5,
            distiller_interval_secs: 3600,
            distiller_idle_secs: 1800,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        timer.update_episode_count(10).await;
        assert_eq!(timer.should_run_distill().await, None);

        // Swap the switch ON via update_config (what AgentCore::rebuild_
        // consolidation_pipeline_if_running does). Same timer object, no
        // task restart. A fresh timer's last_distill_at = now, so the
        // interval gate still applies — verify the policy is live by
        // lowering the interval to 0.
        timer.update_config(SchedulerConfig {
            distiller_enabled: true,
            distiller_accumulation: 5,
            distiller_interval_secs: 0,
            distiller_idle_secs: 1800,
            ..Default::default()
        });
        // Enabled + interval elapsed + backlog ≥ threshold → fires.
        assert_eq!(
            timer.should_run_distill().await,
            Some(TriggerReason::DistillerAccumulation)
        );
        // The snapshot accessor must reflect the swapped policy too.
        assert!(timer.config().distiller_enabled);

        // Swap back OFF → immediately gated again (no spurious run).
        timer.update_config(SchedulerConfig {
            distiller_enabled: false,
            distiller_accumulation: 5,
            distiller_interval_secs: 0,
            distiller_idle_secs: 1800,
            ..Default::default()
        });
        assert_eq!(timer.should_run_distill().await, None);
    }

    #[tokio::test]
    async fn test_distiller_trigger_accumulation() {
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_accumulation: 5,
            distiller_interval_secs: 3600,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        // Backdate the last run so the interval gate is open.
        {
            let mut state = timer.state.lock().await;
            state.last_distill_at = Utc::now() - chrono::TimeDelta::hours(2);
        }
        timer.update_episode_count(10).await;
        assert_eq!(
            timer.should_run_distill().await,
            Some(TriggerReason::DistillerAccumulation)
        );
    }

    #[tokio::test]
    async fn test_distiller_trigger_idle_with_backlog() {
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_accumulation: 100, // backlog below threshold
            distiller_interval_secs: 3600,
            distiller_idle_secs: 1800,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        {
            let mut state = timer.state.lock().await;
            state.last_distill_at = Utc::now() - chrono::TimeDelta::hours(2);
            state.last_active_at = Utc::now() - chrono::TimeDelta::minutes(40);
        }
        timer.update_episode_count(3).await;
        assert_eq!(
            timer.should_run_distill().await,
            Some(TriggerReason::DistillerIdle)
        );
    }

    #[tokio::test]
    async fn test_distiller_trigger_empty_backlog_never_fires() {
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_accumulation: 1,
            distiller_interval_secs: 1,
            distiller_idle_secs: 0,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        timer.update_episode_count(0).await;
        {
            let mut state = timer.state.lock().await;
            state.last_distill_at = Utc::now() - chrono::TimeDelta::hours(2);
            state.last_active_at = Utc::now() - chrono::TimeDelta::hours(2);
        }
        // Interval open + idle open, but no backlog → no run.
        assert_eq!(timer.should_run_distill().await, None);
    }

    #[tokio::test]
    async fn test_consolidation_bg_task_starts_and_stops() {
        let store: Arc<dyn acowork_memory::MemoryProvider> = Arc::new(
            acowork_grafeo::GrafeoStore::new_in_memory().unwrap(),
        );

        struct NoopLlm;
        #[async_trait::async_trait]
        impl TripleExtractorLlm for NoopLlm {
            async fn chat(&self, _messages: Vec<acowork_memory::consolidation::LlmMessage>) -> std::result::Result<acowork_memory::consolidation::LlmResponse, String> {
                Ok(acowork_memory::consolidation::LlmResponse {
                    content: "[]".to_string(),
                    usage_tokens: None,
                })
            }
        }

        let llm: Arc<dyn TripleExtractorLlm> = Arc::new(NoopLlm);
        let embedding_provider: Arc<dyn EmbeddingProvider> = {
            struct DummyEmbeddingProvider;
            #[async_trait::async_trait]
            impl EmbeddingProvider for DummyEmbeddingProvider {
                fn name(&self) -> &str { "dummy" }
                async fn embed(&self, _text: &str) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                    Ok(vec![0.0; 384])
                }
                async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                    Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
                }
                fn dimension(&self) -> usize { 384 }
                async fn is_available(&self) -> bool { true }
            }
            Arc::new(DummyEmbeddingProvider)
        };

        let timer = Arc::new(ConsolidationTimer::new(SchedulerConfig::default()));
        let bg_task = ConsolidationBgTask::spawn(
            timer,
            store,
            llm,
            embedding_provider,
            Duration::from_secs(60),
            None,
        );

        // Give it a moment to start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        bg_task.abort();
    }

    /// Regression for P0 fix: the `ConsolidationTimer` returned from
    /// `start_consolidation_pipeline` must have a functional
    /// `notify_active()` method. AgentCore stores this timer and calls
    /// `notify_active()` on every agent turn to reset the idle timer.
    ///
    /// This test verifies the timer's idle-reset works correctly after
    /// being created and used in a background task context.
    #[tokio::test]
    async fn test_timer_idle_reset_after_consolidation_run() {
        let timer = Arc::new(ConsolidationTimer::new(SchedulerConfig {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            ..Default::default()
        }));

        // Simulate agent activity: notify_active should reset idle.
        timer.notify_active().await;

        // Simulate time passing (1 second).
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Verify idle is small (recently active).
        let idle_secs = {
            let state = timer.state.lock().await;
            (Utc::now() - state.last_active_at).num_seconds()
        };
        assert!(
            idle_secs < 5,
            "Idle should be < 5s after notify_active, got {idle_secs}s"
        );

        // Legacy Pending-count assertions removed with `should_run()` /
        // `update_pending_count()` (ADR-057 §5.3 redesign). Idle / backlog
        // state is still tracked for the distiller's inner gate.
    }

    // -----------------------------------------------------------------------
    // EpisodicDistiller step (ADR-068 M4)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_scheduler_config_distiller_off_by_default() {
        // ADR-068 review revision: distiller_enabled defaults to false — the
        // distiller is opt-in per agent (manifest `[memory.distiller].enabled`).
        let config = SchedulerConfig::default();
        assert!(
            !config.distiller_enabled,
            "Distiller is opt-in: the runtime gates it on the manifest \
             `[memory.distiller].enabled = true`; default must stay OFF"
        );
        assert!(config.distiller_config.is_none());
    }

    #[tokio::test]
    async fn test_scheduler_config_distiller_config_roundtrip() {
        let cfg = DistillerConfig {
            fact_min_evidence: 4,
            ..DistillerConfig::default()
        };
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_config: Some(cfg.clone()),
            ..SchedulerConfig::default()
        };
        assert!(config.distiller_enabled);
        assert_eq!(
            config.distiller_config.as_ref().unwrap().fact_min_evidence,
            4
        );
        // Using ..Default::default() keeps existing fields untouched.
        assert_eq!(config.accumulation_threshold, 50);
        let _ = cfg;
    }

    #[cfg(feature = "grafeo-backend")]
    #[tokio::test]
    async fn test_distiller_step_promotes_facts_when_enabled() {
        use super::distiller_fixture::{
            build_distiller_test_fixture, run_episodic_distiller_step_inner,
        };
        let (provider, llm) = build_distiller_test_fixture();
        let config = SchedulerConfig {
            distiller_enabled: true,
            distiller_config: Some(DistillerConfig::default()),
            ..SchedulerConfig::default()
        };
        let result = run_episodic_distiller_step_inner(provider.as_ref(), &*llm, config).await;
        assert!(result.episodes_scanned >= 2);
        assert_eq!(result.facts_promoted, 1);
    }

    #[cfg(feature = "grafeo-backend")]
    #[tokio::test]
    async fn test_distiller_step_noop_when_disabled() {
        use super::distiller_fixture::{
            build_distiller_test_fixture, run_episodic_distiller_step_inner,
        };
        let (provider, llm) = build_distiller_test_fixture();
        let config = SchedulerConfig {
            distiller_enabled: false,
            ..SchedulerConfig::default()
        };
        // When disabled the step is never invoked; the inner function mirrors
        // the loop's gate: nothing happens.
        let result = run_episodic_distiller_step_inner(provider.as_ref(), &*llm, config).await;
        assert_eq!(result.facts_promoted, 0);
        assert_eq!(result.episodes_scanned, 0);
    }
}

#[cfg(all(test, feature = "grafeo-backend"))]
mod distiller_fixture {
    use super::*;
    use acowork_memory::consolidation::{DistillerResult, LlmMessage, LlmResponse};

    pub struct MockDistillerLlm {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl MockDistillerLlm {
        pub fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockDistillerLlm {
        async fn chat(
            &self,
            _messages: Vec<LlmMessage>,
        ) -> std::result::Result<LlmResponse, String> {
            let resp = self.responses.lock().unwrap().pop_front().unwrap();
            Ok(LlmResponse {
                content: resp,
                usage_tokens: None,
            })
        }
    }

    pub fn build_distiller_test_fixture() -> (Arc<acowork_grafeo::GrafeoStore>, Arc<MockDistillerLlm>) {
        let store = Arc::new(acowork_grafeo::GrafeoStore::new_in_memory().unwrap());
        let provider: Arc<dyn acowork_memory::MemoryProvider> = store.clone();
        // Seed 2 unconsolidated Fact episodes.
        use acowork_memory::types::{Episode, KnowledgeSubType};
        use chrono::Utc;
        for i in 0..2 {
            let ep = Episode {
                session_id: format!("sess-{i}"),
                turn_index: 0,
                role: "user".to_string(),
                content: "User lives in Shanghai".to_string(),
                embedding: None,
                timestamp: Utc::now(),
                consolidated: false,
                metadata: Default::default(),
                importance: 0.5,
                knowledge_subtype: Some(KnowledgeSubType::Fact),
            };
            provider.store_episode(&ep).unwrap();
        }
        // Read back the actual storage ids assigned by the provider so the
        // mock LLM's extraction response references the correct episode_id.
        let raw = provider
            .get_episodes_by_subtype(Some(KnowledgeSubType::Fact), 10)
            .unwrap();
        let ids: Vec<u64> = raw.iter().map(|(id, _)| *id).collect();
        let extraction_items: Vec<String> = ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"episode_id": {id}, "structure": {{"kind":"triple","subject":"user","predicate":"lives_in","object":"Shanghai"}}, "autobio_candidate": null}}"#
                )
            })
            .collect();
        let extraction_resp = format!("[{}]", extraction_items.join(","));
        let llm = Arc::new(MockDistillerLlm::new(vec![
            extraction_resp,
            r#"{"decision":"promote","confidence":0.95,"reasoning":"consistent","merged_content":"user lives_in Shanghai"}"#
                .to_string(),
        ]));
        (store, llm)
    }

    /// Mirrors the gated distiller step; returns the result so tests can
    /// assert on promotion behaviour without running the full loop.
    pub async fn run_episodic_distiller_step_inner(
        provider: &dyn acowork_memory::MemoryProvider,
        llm: &dyn TripleExtractorLlm,
        config: SchedulerConfig,
    ) -> DistillerResult {
        use acowork_grafeo::consolidation::{DefaultEpisodicDistiller, EpisodicDistiller};
        if !config.distiller_enabled {
            return DistillerResult::default();
        }
        let distiller = DefaultEpisodicDistiller;
        let distiller_config = config.distiller_config.unwrap_or_default();
        // No embedding function in the fixture — clustering falls back to
        // exact key equality (episodes share the same triple key).
        distiller
            .run(provider, Some(llm), None, &distiller_config)
            .await
            .expect("distiller run should succeed")
    }
}

// ── ADR-071 W2: embedding bridge (block_in_place / degrade) ────────
//
// Separate test module (the main `tests` module above already closed);
// `build_embedding_bridge` is reachable via `super::super::*`.
#[cfg(test)]
mod embedding_bridge_tests {
    use super::*;

    /// Dummy embedding used to exercise the bridge (pure function, no
    /// runtime dependency).
    struct DummyEmbedding;

    #[async_trait::async_trait]
    impl acowork_core::EmbeddingProvider for DummyEmbedding {
        fn name(&self) -> &str {
            "dummy-bridge-test"
        }
        async fn embed(
            &self,
            text: &str,
        ) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
            Ok(acowork_memory::manager::procedural_embedding_fallback(text))
        }
        async fn embed_batch(
            &self,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
        fn dimension(&self) -> usize {
            384
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    /// W2 regression: on a multi-thread runtime the bridge hands the
    /// embedding call to a blocking worker via `block_in_place` — it must
    /// return `Some` and never panic ("Cannot start a runtime from within a
    /// runtime").
    #[tokio::test(flavor = "multi_thread")]
    async fn test_embedding_bridge_multi_thread_returns_fn_and_works() {
        let bridge = build_embedding_bridge(Arc::new(DummyEmbedding));
        let bridge = bridge.expect("multi-thread runtime must yield a bridge");
        let vec = bridge("user lives in Shanghai");
        assert_eq!(vec.len(), 384, "embedding dimension");
    }

    /// W2 regression: on a current-thread runtime the bridge degrades to
    /// `None` (callers fall back to exact-key clustering) instead of
    /// risking a `block_on` panic inside the single worker.
    #[tokio::test]
    async fn test_embedding_bridge_current_thread_degrades_to_none() {
        let bridge = build_embedding_bridge(Arc::new(DummyEmbedding));
        assert!(
            bridge.is_none(),
            "current-thread runtime must degrade to exact-key clustering"
        );
    }

    /// W2 regression: with NO runtime context at all (pure sync call) the
    /// bridge is `None` — `Handle::try_current()` fails gracefully.
    #[test]
    fn test_embedding_bridge_no_runtime_degrades_to_none() {
        let bridge = build_embedding_bridge(Arc::new(DummyEmbedding));
        assert!(bridge.is_none(), "no runtime context must yield None");
    }
}
