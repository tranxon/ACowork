//! Retry + fallback chain for LLM providers
//!
//! Adapted from zeroclaw/src/providers/reliable.rs
//! Rollball deviation: uses rollball-core Provider trait instead of ZeroClaw's.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use rollball_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderError, ProviderErrorType, StreamEvent,
};
use tokio::time::sleep;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Backoff strategy
    pub backoff: BackoffStrategy,
    /// Maximum wait time in milliseconds
    pub max_wait_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffStrategy::Exponential { base_ms: 1000 },
            max_wait_ms: 30000,
        }
    }
}

/// Backoff strategy for retries
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
    /// Fixed delay between retries
    Fixed { delay_ms: u64 },
    /// Exponential backoff with base delay
    Exponential { base_ms: u64 },
}

impl BackoffStrategy {
    /// Calculate wait duration for a given attempt
    pub fn wait_duration(&self, attempt: u32) -> Duration {
        match self {
            BackoffStrategy::Fixed { delay_ms } => Duration::from_millis(*delay_ms),
            BackoffStrategy::Exponential { base_ms } => {
                let delay = base_ms * 2u64.saturating_pow(attempt);
                Duration::from_millis(delay)
            }
        }
    }
}

/// Reliable provider that wraps another provider with retry and fallback logic
pub struct ReliableProvider {
    /// Primary provider
    primary: Arc<dyn Provider>,
    /// Fallback providers in priority order
    fallbacks: Vec<Arc<dyn Provider>>,
    /// Retry configuration
    retry_config: RetryConfig,
}

impl ReliableProvider {
    /// Create a new reliable provider with retry logic
    pub fn new(primary: Arc<dyn Provider>, retry_config: RetryConfig) -> Self {
        Self {
            primary,
            fallbacks: Vec::new(),
            retry_config,
        }
    }

    /// Add a fallback provider
    pub fn with_fallback(mut self, provider: Arc<dyn Provider>) -> Self {
        self.fallbacks.push(provider);
        self
    }

    /// Check if an error is retryable
    fn is_retryable(error: &rollball_core::RollballError) -> bool {
        match error {
            rollball_core::RollballError::Provider(provider_err) => {
                // Use structured retryable flag; rate-limited, stream-decode,
                // and stream-timeout errors are retryable.
                // ContextOverflow is NOT directly retryable (needs trim first).
                provider_err.retryable
                    || provider_err.error_type == ProviderErrorType::RateLimited
                    || provider_err.error_type == ProviderErrorType::StreamDecodeError
                    || provider_err.error_type == ProviderErrorType::StreamTimeout
            }
            rollball_core::RollballError::RateLimited(_) => true,
            rollball_core::RollballError::Io(_) => true,
            _ => false,
        }
    }

    /// Check if an error indicates insufficient balance (not retryable)
    fn is_balance_exhausted(error: &rollball_core::RollballError) -> bool {
        match error {
            rollball_core::RollballError::Provider(provider_err) => {
                // Common error codes for insufficient balance
                provider_err.message.contains("1113")
                    || provider_err.message.contains("1311")
                    || provider_err.message.contains("insufficient_quota")
            }
            _ => false,
        }
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    fn name(&self) -> &str {
        self.primary.name()
    }

    async fn chat(&self, request: ChatRequest) -> rollball_core::error::Result<ChatResponse> {
        // Try primary provider with retries
        for attempt in 0..self.retry_config.max_attempts {
            match self.primary.chat(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Non-retryable errors — fail immediately
                    if !Self::is_retryable(&e) || Self::is_balance_exhausted(&e) {
                        tracing::error!(error = %e, "Non-retryable error from primary provider");
                        break;
                    }

                    if attempt + 1 < self.retry_config.max_attempts {
                        let wait = self.retry_config.backoff.wait_duration(attempt);
                        let wait = wait.min(Duration::from_millis(self.retry_config.max_wait_ms));
                        tracing::warn!(
                            attempt = attempt + 1,
                            max = self.retry_config.max_attempts,
                            wait_ms = wait.as_millis(),
                            "Retrying primary provider"
                        );
                        sleep(wait).await;
                    } else {
                        tracing::error!(
                            attempts = self.retry_config.max_attempts,
                            "Primary provider retries exhausted"
                        );
                    }
                }
            }
        }

        // Try fallback providers
        for (i, fallback) in self.fallbacks.iter().enumerate() {
            tracing::info!(fallback_index = i, name = %fallback.name(), "Trying fallback provider");
            match fallback.chat(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!(fallback_index = i, error = %e, "Fallback provider failed");
                }
            }
        }

        Err(rollball_core::RollballError::Provider(
            ProviderError::unknown("All providers failed (primary + fallbacks)".to_string()),
        ))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> rollball_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        // Collect all candidate providers: primary + fallbacks
        let candidates: Vec<&Arc<dyn Provider>> = std::iter::once(&self.primary)
            .chain(self.fallbacks.iter())
            .collect();

        for provider in candidates {
            for attempt in 0..self.retry_config.max_attempts {
                match provider.chat_stream(request.clone()).await {
                    Ok(stream) => return Ok(stream),
                    Err(e) if !Self::is_retryable(&e) || Self::is_balance_exhausted(&e) => {
                        tracing::warn!(
                            provider = %provider.name(),
                            error = %e,
                            "Non-retryable stream error, trying next provider"
                        );
                        break; // Move to next provider
                    }
                    Err(e) if attempt + 1 < self.retry_config.max_attempts => {
                        let wait = self.retry_config.backoff.wait_duration(attempt);
                        let wait = wait.min(Duration::from_millis(self.retry_config.max_wait_ms));
                        tracing::warn!(
                            provider = %provider.name(),
                            attempt = attempt + 1,
                            max = self.retry_config.max_attempts,
                            wait_ms = wait.as_millis(),
                            error = %e,
                            "Retrying stream establishment"
                        );
                        sleep(wait).await;
                    }
                    Err(e) => {
                        tracing::error!(
                            provider = %provider.name(),
                            attempts = self.retry_config.max_attempts,
                            error = %e,
                            "Stream retries exhausted for provider"
                        );
                        break; // Try next provider
                    }
                }
            }
        }

        Err(rollball_core::RollballError::Provider(
            ProviderError::network("All providers failed for streaming".to_string()),
        ))
    }

    async fn chat_token_count(
        &self,
        messages: &[ChatMessage],
    ) -> rollball_core::error::Result<u64> {
        self.primary.chat_token_count(messages).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_fixed() {
        let backoff = BackoffStrategy::Fixed { delay_ms: 1000 };
        assert_eq!(backoff.wait_duration(0), Duration::from_millis(1000));
        assert_eq!(backoff.wait_duration(2), Duration::from_millis(1000));
    }

    #[test]
    fn test_backoff_exponential() {
        let backoff = BackoffStrategy::Exponential { base_ms: 1000 };
        assert_eq!(backoff.wait_duration(0), Duration::from_millis(1000));
        assert_eq!(backoff.wait_duration(1), Duration::from_millis(2000));
        assert_eq!(backoff.wait_duration(2), Duration::from_millis(4000));
    }

    #[test]
    fn test_is_retryable() {
        let err = rollball_core::RollballError::Provider(ProviderError::network("timeout".to_string()));
        assert!(ReliableProvider::is_retryable(&err));

        let err = rollball_core::RollballError::Provider(ProviderError::from_status_code(
            401,
            "401 unauthorized".to_string(),
        ));
        assert!(!ReliableProvider::is_retryable(&err));

        let err = rollball_core::RollballError::RateLimited("too many requests".to_string());
        assert!(ReliableProvider::is_retryable(&err));
    }

    #[test]
    fn test_is_balance_exhausted() {
        let err = rollball_core::RollballError::Provider(ProviderError::unknown(
            "insufficient_quota".to_string(),
        ));
        assert!(ReliableProvider::is_balance_exhausted(&err));

        let err = rollball_core::RollballError::Provider(ProviderError::from_status_code(
            500,
            "500 internal error".to_string(),
        ));
        assert!(!ReliableProvider::is_balance_exhausted(&err));
    }
}
