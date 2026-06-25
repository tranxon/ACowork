//! Embedding generation module
//!
//! Provides embedding generation with:
//! - Remote: OpenAI-compatible API (text-embedding-3-small, etc.)
//! - Ollama: local embedding via Ollama's `/api/embed`
//! - ONNX: local embedding via acowork-embed (OpenAI-compatible API)
//! - Extensible via [`EmbeddingProvider`] trait for custom/local backends
//!
//! Fallback chain: ONNX local (500ms) → Ollama (200ms) → Remote API (no timeout).
//! Each provider has its own timeout and consecutive failure tracking.
//! After exceeding the failure threshold, a provider is temporarily skipped.

pub mod ollama;
pub mod remote;

use async_trait::async_trait;
use std::sync::Arc;

/// Embedding generation trait
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Provider name
    fn name(&self) -> &str;

    /// Generate embedding for a single text
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Generate embeddings for multiple texts (batch)
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Get the dimension of embeddings produced by this provider
    fn dimension(&self) -> usize;

    /// Check if this provider is available (e.g., model loaded, API reachable)
    async fn is_available(&self) -> bool;
}

/// Embedding generation errors
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("Local embedding error: {0}")]
    Local(String),

    #[error("Remote embedding error: {0}")]
    Remote(String),

    #[error("Timeout: embedding generation exceeded {0}ms")]
    Timeout(u64),

    #[error("Provider unavailable: {0}")]
    Unavailable(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Configuration for the embedding fallback chain
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    /// Timeout in milliseconds for each embedding request (default: 200)
    pub timeout_ms: u64,
    /// Number of consecutive failures before switching to fallback (default: 2)
    pub failure_threshold: u32,
    /// Whether to prefer local embeddings when available (default: true)
    pub prefer_local: bool,
    /// Maximum batch size for batch embedding (default: 32)
    pub max_batch_size: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 200,
            failure_threshold: 2,
            prefer_local: true,
            max_batch_size: 32,
        }
    }
}

// ── Provider entry (for the providers chain) ────────────────────────────

/// Entry in the provider fallback chain.
///
/// Each provider has its own timeout and consecutive failure counter.
/// After exceeding `failure_threshold` consecutive failures, the provider
/// is temporarily skipped until a subsequent success resets the counter.
struct ProviderEntry {
    provider: Box<dyn EmbeddingProvider>,
    /// Per-provider timeout in milliseconds.
    timeout_ms: u64,
    /// Consecutive failure counter (atomic for thread safety).
    consecutive_failures: std::sync::atomic::AtomicU32,
}

impl ProviderEntry {
    fn new(provider: Box<dyn EmbeddingProvider>, timeout_ms: u64) -> Self {
        Self {
            provider,
            timeout_ms,
            consecutive_failures: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn is_degraded(&self, threshold: u32) -> bool {
        self.consecutive_failures
            .load(std::sync::atomic::Ordering::Relaxed)
            >= threshold
    }

    fn record_failure(&self, threshold: u32) {
        let failures = self
            .consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if failures == threshold {
            tracing::warn!(
                provider = self.provider.name(),
                failures,
                threshold,
                "Embedding provider exceeded failure threshold, temporarily skipping"
            );
        }
    }

    fn record_success(&self) {
        self.consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

// ── FallbackEmbeddingProvider ───────────────────────────────────────────

/// Embedding provider with automatic fallback through a chain of providers.
///
/// # Provider Chain
///
/// The providers are tried in order. For each provider:
/// 1. If it has exceeded `failure_threshold` consecutive failures, skip it.
/// 2. Otherwise, attempt embedding with its per-provider timeout.
/// 3. On success, reset its failure counter and return the result.
/// 4. On failure/timeout, increment its failure counter and try the next provider.
///
/// If all providers fail, return the last error.
///
/// # Backward Compatibility
///
/// The `new(primary, fallback, config)` constructor still works, creating a
/// two-entry providers chain. The new `with_providers()` constructor allows
/// building longer chains (e.g., ONNX → Ollama → Remote).
pub struct FallbackEmbeddingProvider {
    /// Ordered provider chain. Try each in sequence until one succeeds.
    providers: Vec<ProviderEntry>,
    /// Configuration
    config: EmbeddingConfig,
    /// Locked embedding dimension. When set, only providers whose `dimension()`
    /// matches this value are used. This prevents dimension mismatch when the
    /// Grafeo HNSW index has been created with a specific dimension.
    locked_dim: Option<usize>,
}

impl FallbackEmbeddingProvider {
    /// Create a new fallback embedding provider with the classic two-layer pattern.
    ///
    /// This is backward-compatible with the previous API.
    /// Internally converts to a providers chain with two entries.
    pub fn new(
        primary: Option<Box<dyn EmbeddingProvider>>,
        fallback: Box<dyn EmbeddingProvider>,
        config: EmbeddingConfig,
    ) -> Self {
        let mut providers = Vec::new();

        if let Some(primary) = primary {
            providers.push(ProviderEntry::new(primary, config.timeout_ms));
        }
        providers.push(ProviderEntry::new(fallback, 5000)); // Remote: 5s timeout

        Self { providers, config, locked_dim: None }
    }

    /// Create with a full providers chain.
    ///
    /// Each `(provider, timeout_ms)` tuple defines one entry in the chain.
    /// Providers are tried in the order given.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let fallback = FallbackEmbeddingProvider::with_providers(
    ///     vec![
    ///         (Box::new(onnx_provider), 500),     // ONNX local: 500ms timeout
    ///         (Box::new(ollama_provider), 200),    // Ollama: 200ms timeout
    ///         (Box::new(remote_provider), 5000),   // Remote API: 5s timeout
    ///     ],
    ///     EmbeddingConfig::default(),
    /// );
    /// ```
    pub fn with_providers(
        providers: Vec<(Box<dyn EmbeddingProvider>, u64)>,
        config: EmbeddingConfig,
    ) -> Self {
        let providers = providers
            .into_iter()
            .map(|(provider, timeout_ms)| ProviderEntry::new(provider, timeout_ms))
            .collect();
        Self { providers, config, locked_dim: None }
    }

    /// Create with only a remote fallback (no local provider).
    pub fn remote_only(fallback: Box<dyn EmbeddingProvider>) -> Self {
        Self::new(None, fallback, EmbeddingConfig::default())
    }

    /// Lock the provider chain to a specific embedding dimension.
    ///
    /// When set, `embed()` and `embed_batch()` will skip any provider whose
    /// `dimension()` does not match `dim`. This is critical when a Grafeo
    /// HNSW index has been created with a fixed dimension: using a provider
    /// with a different dimension would corrupt the index.
    ///
    /// Returns `self` for chaining.
    pub fn with_locked_dimension(mut self, dim: usize) -> Self {
        self.locked_dim = Some(dim);
        self
    }

    /// Update the locked dimension at runtime (e.g., after migration).
    pub fn set_locked_dimension(&mut self, dim: usize) {
        self.locked_dim = Some(dim);
    }

    /// Returns `true` if the provider's dimension matches the locked dimension
    /// (or if no dimension is locked).
    fn dimension_matches(&self, entry: &ProviderEntry) -> bool {
        match self.locked_dim {
            Some(dim) => entry.provider.dimension() == dim,
            None => true,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for FallbackEmbeddingProvider {
    fn name(&self) -> &str {
        // Return the name of the first non-degraded provider
        for entry in &self.providers {
            if !entry.is_degraded(self.config.failure_threshold) {
                return entry.provider.name();
            }
        }
        // All degraded — return the last provider's name
        self.providers
            .last()
            .map(|e| e.provider.name())
            .unwrap_or("fallback(empty)")
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut last_error =
            EmbeddingError::Unavailable("All embedding providers failed".to_string());

        for entry in &self.providers {
            // Skip degraded providers
            if entry.is_degraded(self.config.failure_threshold) {
                continue;
            }
            // Skip providers with mismatched dimension
            if !self.dimension_matches(entry) {
                tracing::debug!(
                    provider = entry.provider.name(),
                    provider_dim = entry.provider.dimension(),
                    locked_dim = self.locked_dim,
                    "Skipping provider: dimension mismatch"
                );
                continue;
            }

            match tokio::time::timeout(
                std::time::Duration::from_millis(entry.timeout_ms),
                entry.provider.embed(text),
            )
            .await
            {
                Ok(Ok(embedding)) => {
                    entry.record_success();
                    return Ok(embedding);
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        provider = entry.provider.name(),
                        error = %e,
                        "Embedding provider failed"
                    );
                    entry.record_failure(self.config.failure_threshold);
                    last_error = e;
                }
                Err(_) => {
                    tracing::warn!(
                        provider = entry.provider.name(),
                        timeout_ms = entry.timeout_ms,
                        "Embedding provider timed out"
                    );
                    entry.record_failure(self.config.failure_threshold);
                    last_error = EmbeddingError::Timeout(entry.timeout_ms);
                }
            }
        }

        tracing::error!(error = %last_error, "All embedding providers failed");
        Err(last_error)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.config.max_batch_size) {
            let mut chunk_ok = false;
            let mut last_err =
                EmbeddingError::Unavailable("All embedding providers failed".to_string());

            for entry in &self.providers {
                // Skip degraded providers
                if entry.is_degraded(self.config.failure_threshold) {
                    continue;
                }
                // Skip providers with mismatched dimension
                if !self.dimension_matches(entry) {
                    continue;
                }

                // Scale timeout by chunk size
                let timeout = entry.timeout_ms * chunk.len().max(1) as u64;

                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout),
                    entry.provider.embed_batch(chunk),
                )
                .await
                {
                    Ok(Ok(embeddings)) => {
                        entry.record_success();
                        all_embeddings.extend(embeddings);
                        chunk_ok = true;
                        break; // This chunk succeeded, move to next chunk
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            provider = entry.provider.name(),
                            error = %e,
                            "Batch embedding provider failed"
                        );
                        entry.record_failure(self.config.failure_threshold);
                        last_err = e;
                    }
                    Err(_) => {
                        tracing::warn!(
                            provider = entry.provider.name(),
                            "Batch embedding provider timed out"
                        );
                        entry.record_failure(self.config.failure_threshold);
                        last_err = EmbeddingError::Timeout(timeout);
                    }
                }
            }

            if !chunk_ok {
                tracing::error!(error = %last_err, "All embedding providers failed for batch chunk");
                return Err(last_err);
            }
        }

        // Validate we got the right number of embeddings
        if all_embeddings.len() != texts.len() {
            return Err(EmbeddingError::Unavailable(format!(
                "Expected {} embeddings, got {}",
                texts.len(),
                all_embeddings.len()
            )));
        }

        Ok(all_embeddings)
    }

    fn dimension(&self) -> usize {
        // When a dimension is locked, return it directly.
        if let Some(dim) = self.locked_dim {
            return dim;
        }
        // Return the dimension of the first non-degraded provider.
        // (Grafeo index was initialised with this value.)
        // When all providers are degraded, return the last provider's dimension.
        for entry in &self.providers {
            if !entry.is_degraded(self.config.failure_threshold) {
                return entry.provider.dimension();
            }
        }
        self.providers
            .last()
            .map(|e| e.provider.dimension())
            .unwrap_or(0)
    }

    async fn is_available(&self) -> bool {
        for entry in &self.providers {
            if !entry.is_degraded(self.config.failure_threshold)
                && entry.provider.is_available().await
            {
                return true;
            }
        }
        false
    }
}

// ── Arc delegate wrapper ────────────────────────────────────────────────

/// Wraps an `Arc<dyn EmbeddingProvider>` into a `Box<dyn EmbeddingProvider>`.
///
/// This is needed when we want to use an existing shared provider as a
/// fallback entry in a new `FallbackEmbeddingProvider` chain. The `new()`
/// constructor requires `Box<dyn EmbeddingProvider>`, but `AgentCore`
/// stores providers as `Arc<dyn EmbeddingProvider>`.
pub struct ArcDelegateEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
}

impl ArcDelegateEmbeddingProvider {
    /// Create a new delegate from an Arc.
    pub fn from_arc(arc: Arc<dyn EmbeddingProvider>) -> Self {
        Self { inner: arc }
    }
}

#[async_trait]
impl EmbeddingProvider for ArcDelegateEmbeddingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.inner.embed(text).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.inner.embed_batch(texts).await
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    async fn is_available(&self) -> bool {
        self.inner.is_available().await
    }
}
