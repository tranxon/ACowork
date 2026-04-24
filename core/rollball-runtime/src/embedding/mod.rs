//! Embedding generation module
//!
//! Provides embedding generation with two backends:
//! - Local (feature-gated: `local-embeddings`): Uses ONNX Runtime + all-MiniLM-L6-v2
//! - Remote: Uses OpenAI text-embedding-3-small as fallback
//!
//! Automatic switching: local → remote when local fails or is unavailable.
//! Remote fallback triggers after 200ms timeout or 2 consecutive failures.

pub mod remote;

#[cfg(feature = "local-embeddings")]
pub mod local;

use async_trait::async_trait;

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

/// Embedding provider with automatic fallback
pub struct FallbackEmbeddingProvider {
    /// Primary provider (usually local)
    primary: Option<Box<dyn EmbeddingProvider>>,
    /// Fallback provider (usually remote)
    fallback: Box<dyn EmbeddingProvider>,
    /// Configuration
    config: EmbeddingConfig,
    /// Consecutive failure count for primary provider
    primary_failures: std::sync::atomic::AtomicU32,
}

impl FallbackEmbeddingProvider {
    /// Create a new fallback embedding provider
    pub fn new(
        primary: Option<Box<dyn EmbeddingProvider>>,
        fallback: Box<dyn EmbeddingProvider>,
        config: EmbeddingConfig,
    ) -> Self {
        Self {
            primary,
            fallback,
            config,
            primary_failures: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Create with only a remote fallback (no local provider)
    pub fn remote_only(fallback: Box<dyn EmbeddingProvider>) -> Self {
        Self::new(None, fallback, EmbeddingConfig::default())
    }

    /// Record a primary provider failure
    fn record_primary_failure(&self) {
        let failures = self.primary_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if failures >= self.config.failure_threshold {
            tracing::warn!(
                failures,
                threshold = self.config.failure_threshold,
                "Primary embedding provider exceeded failure threshold, switching to fallback"
            );
        }
    }

    /// Record a primary provider success (reset failure counter)
    fn record_primary_success(&self) {
        self.primary_failures.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Check if primary provider should be skipped due to failures
    fn should_skip_primary(&self) -> bool {
        self.primary_failures.load(std::sync::atomic::Ordering::Relaxed) >= self.config.failure_threshold
    }
}

#[async_trait]
impl EmbeddingProvider for FallbackEmbeddingProvider {
    fn name(&self) -> &str {
        if self.primary.is_some() && !self.should_skip_primary() {
            "fallback(primary=local)"
        } else {
            "fallback(primary=remote)"
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Try primary provider first
        if let Some(ref primary) = self.primary {
            if !self.should_skip_primary() {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(self.config.timeout_ms),
                    primary.embed(text),
                )
                .await
                {
                    Ok(Ok(embedding)) => {
                        self.record_primary_success();
                        return Ok(embedding);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "Primary embedding provider failed");
                        self.record_primary_failure();
                    }
                    Err(_) => {
                        tracing::warn!(timeout_ms = self.config.timeout_ms, "Primary embedding provider timed out");
                        self.record_primary_failure();
                    }
                }
            }
        }

        // Fallback to remote provider
        self.fallback.embed(text).await.map_err(|e| {
            tracing::error!(error = %e, "Fallback embedding provider also failed");
            e
        })
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        // Process in batches
        let mut all_embeddings = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.config.max_batch_size) {
            // Try primary provider first
            if let Some(ref primary) = self.primary {
                if !self.should_skip_primary() {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(self.config.timeout_ms * chunk.len() as u64),
                        primary.embed_batch(chunk),
                    )
                    .await
                    {
                        Ok(Ok(embeddings)) => {
                            self.record_primary_success();
                            all_embeddings.extend(embeddings);
                            continue;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "Primary batch embedding failed");
                            self.record_primary_failure();
                        }
                        Err(_) => {
                            tracing::warn!("Primary batch embedding timed out");
                            self.record_primary_failure();
                        }
                    }
                }
            }

            // Fallback to remote provider
            let embeddings = self.fallback.embed_batch(chunk).await?;
            all_embeddings.extend(embeddings);
        }

        Ok(all_embeddings)
    }

    fn dimension(&self) -> usize {
        if let Some(ref primary) = self.primary {
            primary.dimension()
        } else {
            self.fallback.dimension()
        }
    }

    async fn is_available(&self) -> bool {
        if let Some(ref primary) = self.primary {
            primary.is_available().await
        } else {
            self.fallback.is_available().await
        }
    }
}
