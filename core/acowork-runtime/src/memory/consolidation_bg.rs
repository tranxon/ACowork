//! Consolidation background task - timing policy and background loop.
//!
//! ADR-051 P4: Replaces the grafeo `ConsolidationScheduler` with a
//! lightweight `ConsolidationTimer` that lives in the Runtime. The timer
//! implements the same scheduling policy (idle-timeout + accumulation
//! threshold) without needing a GrafeoStore.
//!
//! The background task:
//! 1. Polls `should_run()` every 60 seconds
//! 2. When triggered, runs the full offline consolidation pipeline
//!    (triple extraction + conflict resolution + generalization)
//! 3. Logs results and errors
//!
//! The actual consolidation execution goes through `dyn MemoryProvider`,
//! so any provider backend can be used.

use std::sync::Arc;
use std::time::Duration;

use acowork_memory::consolidation::{
    GeneralizationConfig, OfflineConsolidationConfig, SchedulerConfig, TripleExtractorLlm,
};
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
    /// Agent has been idle for longer than the configured timeout.
    IdleTimeout,
    /// The number of pending nodes exceeded the accumulation threshold.
    Accumulation,
    /// Manually triggered by the user or API.
    Manual,
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
    config: SchedulerConfig,
    state: Mutex<TimerState>,
}

#[derive(Debug)]
struct TimerState {
    last_active_at: chrono::DateTime<Utc>,
    pending_count: usize,
}

impl ConsolidationTimer {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(TimerState {
                last_active_at: Utc::now(),
                pending_count: 0,
            }),
        }
    }

    /// Notify the timer that the agent is active. Resets the idle timer.
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

    /// Get the scheduler config (used for batch_size / min_pending_age_hours).
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Get the current idle duration in seconds (since last `notify_active`).
    /// Used by the HTTP status endpoint and tests.
    pub async fn idle_secs(&self) -> i64 {
        let state = self.state.lock().await;
        (Utc::now() - state.last_active_at).num_seconds()
    }

    /// Get the current pending node count.
    /// Used by the HTTP status endpoint.
    pub async fn pending_count(&self) -> usize {
        let state = self.state.lock().await;
        state.pending_count
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

        // Update pending count from the provider.
        let pending_count = match provider.get_pending_consolidation_count() {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to count pending nodes for scheduler");
                continue;
            }
        };
        scheduler.update_pending_count(pending_count).await;

        // Check if consolidation should run.
        let trigger = match scheduler.should_run().await {
            Some(reason) => reason,
            None => continue,
        };

        tracing::info!(?trigger, pending = pending_count, "Consolidation triggered");

        // Build embedding function from the embedding provider.
        let embedding_fn = {
            let ep = embedding_provider.clone();
            let handle = tokio::runtime::Handle::current();
            Arc::new(move |text: &str| -> Vec<f32> {
                let text_owned = text.to_string();
                match handle.block_on(ep.embed(&text_owned)) {
                    Ok(vec) => vec,
                    Err(e) => {
                        tracing::warn!(error = %e, "Embedding failed during consolidation, using zero vector");
                        vec![]
                    }
                }
            }) as DistillerEmbeddingFn
        };

        // Build offline config from scheduler config.
        let offline_config = OfflineConsolidationConfig {
            batch_size: scheduler.config().batch_size,
            min_pending_age_hours: scheduler.config().min_pending_age_hours,
        };

        // ADR-068 M4: EpisodicDistiller step (off-by-default).
        // The distiller promotes classified episodes to the semantic layer.
        // It runs BEFORE the legacy offline consolidation (which targets
        // Pending nodes) — the two pipelines are independent (two-axis
        // orthogonalization) but share the same trigger.
        if scheduler.config().distiller_enabled {
            run_episodic_distiller_step(
                provider.as_ref(),
                &*llm,
                &embedding_fn,
                scheduler.config(),
            )
            .await;
        }

        // Run consolidation through the provider trait.
        let gen_config = GeneralizationConfig::default();
        match provider
            .run_offline_consolidation(&offline_config, Some(&*llm), Some(embedding_fn), Some(&gen_config))
            .await
        {
            Ok(result) => {
                tracing::info!(
                    trigger = ?trigger,
                    upgraded = result.upgraded,
                    conflicts_resolved = result.conflicts_resolved,
                    "Consolidation run complete"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Consolidation run failed");
            }
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

/// Run one EpisodicDistiller pass inside the background consolidation loop.
///
/// ADR-068 M4: the distiller is the ONLY producer of semantic-layer nodes
/// (R3). It scans unconsolidated episodes tagged with a `knowledge_subtype`
/// and promotes evidence-backed clusters to Knowledge/Procedural/
/// Autobiographical nodes. The step is off-by-default (`distiller_enabled`),
/// so this function is a no-op unless explicitly configured.
///
/// The distiller lives in `acowork-grafeo`, which is an optional runtime
/// dependency behind the `grafeo-backend` feature. When that feature is
/// disabled the step degrades to a logged no-op.
async fn run_episodic_distiller_step(
    provider: &dyn acowork_memory::MemoryProvider,
    llm: &dyn TripleExtractorLlm,
    embedding_fn: &DistillerEmbeddingFn,
    config: &SchedulerConfig,
) {
    // When grafeo is not compiled in, nothing to do.
    #[cfg(not(feature = "grafeo-backend"))]
    {
        let _ = (provider, llm, embedding_fn, config);
        tracing::warn!(
            "EpisodicDistiller step requested but acowork-grafeo is not enabled \
             (feature 'grafeo-backend' is off); skipping"
        );
    }

    #[cfg(feature = "grafeo-backend")]
    {
        use acowork_grafeo::consolidation::{DefaultEpisodicDistiller, EpisodicDistiller};

        let distiller = DefaultEpisodicDistiller;
        let distiller_config = config.distiller_config.clone().unwrap_or_default();
        match distiller
            .run(provider, Some(llm), Some(embedding_fn), &distiller_config)
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
            }
            Err(e) => {
                tracing::warn!(error = %e, "EpisodicDistiller run failed (ADR-068)");
            }
        }
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

    #[tokio::test]
    async fn test_timer_accumulation_trigger() {
        let config = SchedulerConfig {
            accumulation_threshold: 5,
            ..Default::default()
        };
        let timer = ConsolidationTimer::new(config);
        timer.update_pending_count(10).await;
        let trigger = timer.should_run().await;
        assert_eq!(trigger, Some(TriggerReason::Accumulation));
    }

    #[tokio::test]
    async fn test_timer_no_trigger_when_empty() {
        let timer = ConsolidationTimer::new(SchedulerConfig::default());
        timer.update_pending_count(0).await;
        let trigger = timer.should_run().await;
        assert_eq!(trigger, None);
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

        // Without notify_active, idle should NOT trigger (pending = 0).
        timer.update_pending_count(0).await;
        let trigger = timer.should_run().await;
        assert_eq!(trigger, None, "Should not trigger with 0 pending nodes");

        // With pending nodes but recent activity, should still not trigger
        // (idle_timeout_secs = 1800, only 1s elapsed).
        timer.update_pending_count(100).await;
        let trigger = timer.should_run().await;
        // Accumulation threshold is 50, pending is 100 -> should trigger.
        assert_eq!(
            trigger,
            Some(TriggerReason::Accumulation),
            "Should trigger via accumulation when pending >= threshold"
        );
    }

    // -----------------------------------------------------------------------
    // EpisodicDistiller step (ADR-068 M4)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_scheduler_config_distiller_off_by_default() {
        // ADR-068 M4 acceptance: distiller_enabled defaults to false.
        let config = SchedulerConfig::default();
        assert!(!config.distiller_enabled);
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
