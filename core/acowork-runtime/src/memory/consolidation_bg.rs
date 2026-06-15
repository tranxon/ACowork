//! Consolidation background task — bridges the ConsolidationScheduler
//! (grafeo crate) to the Runtime's tokio runtime.
//!
//! The scheduler itself is a pure data structure in grafeo that only
//! answers `should_run()` and `run_now()`. This module spawns a
//! background tokio task that:
//!
//! 1. Polls `should_run()` every 60 seconds
//! 2. When triggered, runs the full offline consolidation pipeline
//!    (triple extraction + conflict resolution + generalization)
//! 3. Logs results and errors
//!
//! The task holds Arc references to AgentCore resources so it
//! doesn't prevent shutdown.

use std::sync::Arc;
use std::time::Duration;

use acowork_grafeo::consolidation::triple_extraction::TripleExtractorLlm;
use acowork_grafeo::consolidation::{
    ConsolidationScheduler, GeneralizationConfig, OfflineConsolidationConfig, SchedulerConfig,
};
use acowork_grafeo::grafeo::GrafeoStore;
use tokio::sync::Mutex;

use crate::embedding::EmbeddingProvider;
use crate::memory::llm_adapter::ProviderLlmAdapter;

// ---------------------------------------------------------------------------
// Background task handle
// ---------------------------------------------------------------------------

/// Handle for the background consolidation task.
///
/// Dropping this handle cancels the background task (via `JoinHandle::abort`).
#[derive(Debug)]
pub struct ConsolidationBgTask {
    /// The tokio JoinHandle — abort on drop.
    join_handle: tokio::task::JoinHandle<()>,
}

impl ConsolidationBgTask {
    /// Spawn the background consolidation task.
    pub fn spawn(
        scheduler: Arc<ConsolidationScheduler>,
        store: Arc<GrafeoStore>,
        llm: Arc<dyn TripleExtractorLlm>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        poll_interval: Duration,
        work_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let join_handle = tokio::spawn(async move {
            run_consolidation_loop(
                scheduler,
                store,
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
    scheduler: Arc<ConsolidationScheduler>,
    store: Arc<GrafeoStore>,
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
    // First tick fires immediately — skip it so we don't consolidate on startup.
    interval.tick().await;

    loop {
        interval.tick().await;

        // Update pending count from the store.
        // GrafeoStore is Sync, so we can call methods directly on the Arc.
        let pending_count = match store.get_pending_for_consolidation(0, 10_000) {
            Ok(nodes) => nodes.len(),
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

        tracing::info!(?trigger, pending_count, "Consolidation scheduler triggered");

        // P3 T4.4: Simple global lock — if another agent is running consolidation,
        // skip this cycle. Uses a file-based lock in the work directory.
        let lock_path = work_dir
            .as_ref()
            .map(|d| d.join("memory").join(".consolidation.lock"));
        let lock_held = match &lock_path {
            Some(path) => acquire_consolidation_lock(path),
            None => true, // No lock file → always allow (single-agent mode)
        };

        if !lock_held {
            tracing::info!("Consolidation lock held by another agent, skipping this cycle");
            continue;
        }

        // Create a Send+Sync embedding closure wrapped in Arc.
        // Uses tokio::runtime::Handle::current().block_on() inside spawn_blocking
        // to avoid creating a new thread + tokio Runtime for every embedding call.
        // The consolidation API requires a synchronous closure, so we bridge
        // async → sync via Handle::current() on a blocking thread.
        let emb_provider = embedding_provider.clone();
        let embedding_fn: Arc<dyn Fn(&str) -> Vec<f32> + Send + Sync> = Arc::new(
            move |text: &str| -> Vec<f32> {
                let provider = emb_provider.clone();
                let text_owned = text.to_string();
                // Use the current tokio runtime handle to block_on the async embed call.
                // This is safe because the closure is called from within a tokio task
                // (the consolidation loop), and Handle::current() references the
                // same runtime without creating a new one.
                // Note: block_on inside an async context would panic, but our
                // Grafeo consolidation pipeline calls this closure synchronously
                // from within run_offline_consolidation, which is already async.
                // We use tokio::task::block_in_place to safely block on the
                // current thread without panicking.
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    match handle.block_on(provider.embed(&text_owned)) {
                        Ok(vec) => vec,
                        Err(e) => {
                            tracing::warn!(error = %e, "Embedding failed in consolidation, using zero vector");
                            vec![0.0f32; provider.dimension()]
                        }
                    }
                })
            },
        );

        // Run consolidation with generalization.
        let offline_config = OfflineConsolidationConfig {
            batch_size: scheduler.config().batch_size,
            min_pending_age_hours: scheduler.config().min_pending_age_hours,
        };
        let gen_config = GeneralizationConfig::default();

        let result = store
            .run_offline_consolidation_with_generalization(
                &offline_config,
                Some(llm.as_ref()),
                Some(embedding_fn),
                Some(&gen_config),
            )
            .await;

        match result {
            Ok(result) => {
                tracing::info!(
                    trigger = ?trigger,
                    upgraded = result.upgraded,
                    kept_pending = result.kept_pending,
                    marked_dormant = result.marked_dormant,
                    procedural_created = result.procedural_created,
                    procedural_boosted = result.procedural_boosted,
                    history_compressed = result.history_compressed,
                    episodic_cleaned = result.episodic_cleaned,
                    "Offline consolidation completed"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "Offline consolidation failed");
            }
        }

        // After running, update pending count again.
        let new_pending = match store.get_pending_for_consolidation(0, 10_000) {
            Ok(nodes) => nodes.len(),
            Err(_) => 0,
        };
        scheduler.update_pending_count(new_pending).await;

        // Release the consolidation lock.
        if let Some(ref path) = lock_path {
            release_consolidation_lock(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Global consolidation lock (file-based)
// ---------------------------------------------------------------------------

/// Try to acquire the consolidation lock.
///
/// Returns `true` if the lock was acquired, `false` if another agent
/// holds it. Uses a simple file with PID + timestamp.
fn acquire_consolidation_lock(lock_path: &std::path::Path) -> bool {
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Check if lock file exists and is recent (< 10 minutes old).
    if let Ok(metadata) = std::fs::metadata(lock_path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(duration) = modified.elapsed() {
                if duration.as_secs() < 600 {
                    // Lock file is recent — another agent is likely running.
                    return false;
                }
            }
        }
        // Lock file is stale — remove it.
        let _ = std::fs::remove_file(lock_path);
    }

    // Write our PID + timestamp.
    let content = format!(
        "pid={}\ntime={}\n",
        std::process::id(),
        chrono::Utc::now().to_rfc3339()
    );
    std::fs::write(lock_path, content).is_ok()
}

/// Release the consolidation lock by removing the lock file.
fn release_consolidation_lock(lock_path: &std::path::Path) {
    let _ = std::fs::remove_file(lock_path);
}

// ---------------------------------------------------------------------------
// Builder — creates scheduler + bg task from AgentCore resources
// ---------------------------------------------------------------------------

/// Parameters needed to create the consolidation background pipeline.
pub struct ConsolidationParams {
    /// GrafeoStore (shared, already initialized).
    pub store: Arc<GrafeoStore>,
    /// LLM Provider for triple extraction and conflict resolution.
    pub provider: Arc<dyn acowork_core::providers::traits::Provider>,
    /// Model name for the LLM adapter.
    pub model: String,
    /// Embedding provider for generalization.
    pub embedding_provider: Arc<dyn EmbeddingProvider>,
    /// Scheduler configuration.
    pub scheduler_config: SchedulerConfig,
    /// Poll interval for the background task.
    pub poll_interval: Duration,
    /// Working directory for the lock file.
    /// If None, no file-based lock is used (single-agent mode).
    pub work_dir: Option<std::path::PathBuf>,
}

/// Create and start the consolidation background pipeline.
///
/// Returns the scheduler (for `notify_active()` calls) and the
/// background task handle (to be stored in AgentCore).
///
/// Note: The ConsolidationScheduler in grafeo expects `Arc<Mutex<GrafeoStore>>`
/// for its `run_now()` method. However, our background loop runs consolidation
/// directly on the shared `Arc<GrafeoStore>` (which is Sync). We create a
/// lightweight Mutex wrapper solely for the scheduler's constructor, but the
/// actual consolidation is done through the direct Arc reference.
pub fn start_consolidation_pipeline(
    params: ConsolidationParams,
) -> (Arc<ConsolidationScheduler>, ConsolidationBgTask) {
    // Create a Mutex-wrapped clone-like reference for the scheduler.
    // The scheduler stores this for its `run_now()` method, but our
    // background loop uses the direct Arc<GrafeoStore> instead.
    // We create a new in-memory store as a placeholder for the scheduler's
    // internal state — the scheduler's `run_now()` is not used by our
    // background task; we call the store methods directly.
    let scheduler_store = Arc::new(Mutex::new(
        GrafeoStore::new_in_memory().expect("in-memory store for scheduler should not fail"),
    ));

    // Create the LLM adapter.
    let llm_adapter = Arc::new(ProviderLlmAdapter::new(params.provider, params.model));

    // Create the scheduler (uses its own store for `run_now()`, but we
    // use the shared store for our direct consolidation calls).
    let scheduler = Arc::new(ConsolidationScheduler::new(
        scheduler_store,
        params.scheduler_config,
    ));

    // Spawn the background task with the REAL store.
    let bg_task = ConsolidationBgTask::spawn(
        scheduler.clone(),
        params.store,
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
    use acowork_grafeo::consolidation::triple_extraction::{LlmMessage, LlmResponse};
    use acowork_grafeo::types::DEFAULT_EMBEDDING_DIM;

    /// A mock TripleExtractorLlm that returns a fixed empty JSON array.
    struct MockExtractorLlm;

    #[async_trait::async_trait]
    impl TripleExtractorLlm for MockExtractorLlm {
        async fn chat(
            &self,
            _messages: Vec<LlmMessage>,
        ) -> std::result::Result<LlmResponse, String> {
            Ok(LlmResponse {
                content: "[]".to_string(),
                usage_tokens: Some(50),
            })
        }
    }

    /// A mock EmbeddingProvider for testing.
    struct MockEmbedding;

    #[async_trait::async_trait]
    impl EmbeddingProvider for MockEmbedding {
        fn name(&self) -> &str {
            "mock"
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
            Ok(vec![0.1f32; DEFAULT_EMBEDDING_DIM])
        }
        async fn embed_batch(
            &self,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbeddingError> {
            Ok(texts
                .iter()
                .map(|_| vec![0.1f32; DEFAULT_EMBEDDING_DIM])
                .collect())
        }
        fn dimension(&self) -> usize {
            DEFAULT_EMBEDDING_DIM
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_consolidation_bg_task_starts_and_stops() {
        let store = Arc::new(GrafeoStore::new_in_memory().unwrap());
        let scheduler_store = Arc::new(Mutex::new(GrafeoStore::new_in_memory().unwrap()));

        let scheduler = Arc::new(ConsolidationScheduler::new(
            scheduler_store,
            SchedulerConfig {
                idle_timeout_secs: 1,
                accumulation_threshold: 999,
                batch_size: 50,
                min_pending_age_hours: 1,
            },
        ));

        let llm = Arc::new(MockExtractorLlm);
        let embedding = Arc::new(MockEmbedding);

        let bg = ConsolidationBgTask::spawn(
            scheduler.clone(),
            store,
            llm,
            embedding,
            Duration::from_secs(1),
            None, // No lock file in tests
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(bg);
    }

    #[tokio::test]
    async fn test_scheduler_notify_active() {
        let scheduler_store = Arc::new(Mutex::new(GrafeoStore::new_in_memory().unwrap()));
        let scheduler = Arc::new(ConsolidationScheduler::new(
            scheduler_store,
            SchedulerConfig::default(),
        ));

        scheduler.notify_active().await;
        let idle = scheduler.idle_seconds().await;
        assert!(idle < 5, "Idle should be near 0 after notify_active");
    }
}
