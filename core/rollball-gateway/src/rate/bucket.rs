//! Token bucket rate limiter
//!
//! S4.4: Per-provider token bucket implementation with:
//! - Configurable capacity and refill rate
//! - Retryable rate limiting (429 + retry_after)
//! - Non-retryable rate limiting (insufficient balance)
//! - Fair scheduling across multiple agents

use std::collections::HashMap;
use std::time::Instant;

/// Result of a rate acquire attempt
#[derive(Debug, Clone)]
pub struct RateResult {
    /// Whether the token was granted
    pub granted: bool,
    /// If not granted, milliseconds until retry is possible
    pub retry_after_ms: Option<u64>,
    /// Whether the denial is retryable (true = 429-style, false = permanent)
    pub retryable: bool,
}

/// Token bucket for a single provider
///
/// S4.4.1: Token Bucket implementation per provider.
/// Each bucket has a maximum capacity and a refill rate.
/// Tokens are consumed on each request and refilled over time.
#[derive(Debug)]
pub struct TokenBucket {
    /// Provider name
    pub provider: String,
    /// Maximum number of tokens in the bucket
    pub capacity: u64,
    /// Current number of available tokens
    pub tokens: f64,
    /// Tokens refilled per second
    pub refill_per_sec: f64,
    /// Last refill timestamp
    pub last_refill: Instant,
    /// Per-agent consumption tracking for fair scheduling
    pub agent_usage: HashMap<String, u64>,
}

impl TokenBucket {
    /// Create a new token bucket
    pub fn new(provider: &str, capacity: u64, refill_per_sec: f64) -> Self {
        Self {
            provider: provider.to_string(),
            capacity,
            tokens: capacity as f64,
            refill_per_sec,
            last_refill: Instant::now(),
            agent_usage: HashMap::new(),
        }
    }

    /// Refill tokens based on elapsed time
    pub fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let tokens_to_add = elapsed * self.refill_per_sec;
        self.tokens = (self.tokens + tokens_to_add).min(self.capacity as f64);
        self.last_refill = now;
    }

    /// Try to acquire a token (non-blocking)
    ///
    /// Returns:
    /// - RateResult { granted: true, .. } if token is available
    /// - RateResult { granted: false, retry_after_ms: Some(ms), retryable: true }
    ///   if the bucket is temporarily empty but will refill (S4.4.3: 429 + retry_after)
    /// - RateResult { granted: false, retry_after_ms: None, retryable: false }
    ///   if the bucket is permanently empty (S4.4.4: insufficient balance)
    pub fn try_acquire(&mut self, agent_id: &str) -> RateResult {
        self.refill();

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            *self.agent_usage.entry(agent_id.to_string()).or_insert(0) += 1;
            RateResult {
                granted: true,
                retry_after_ms: None,
                retryable: true,
            }
        } else if self.refill_per_sec > 0.0 {
            // S4.4.3: Retryable — bucket will refill
            let tokens_needed = 1.0 - self.tokens;
            let wait_secs = tokens_needed / self.refill_per_sec;
            let wait_ms = (wait_secs * 1000.0).ceil() as u64;
            RateResult {
                granted: false,
                retry_after_ms: Some(wait_ms.max(100)), // Minimum 100ms
                retryable: true,
            }
        } else {
            // S4.4.4: Non-retryable — no refill, bucket is permanently empty
            RateResult {
                granted: false,
                retry_after_ms: None,
                retryable: false,
            }
        }
    }

    /// Try to acquire multiple tokens at once
    pub fn try_acquire_n(&mut self, n: u64, agent_id: &str) -> RateResult {
        self.refill();

        if self.tokens >= n as f64 {
            self.tokens -= n as f64;
            *self.agent_usage.entry(agent_id.to_string()).or_insert(0) += n;
            RateResult {
                granted: true,
                retry_after_ms: None,
                retryable: true,
            }
        } else if self.refill_per_sec > 0.0 {
            let tokens_needed = n as f64 - self.tokens;
            let wait_secs = tokens_needed / self.refill_per_sec;
            let wait_ms = (wait_secs * 1000.0).ceil() as u64;
            RateResult {
                granted: false,
                retry_after_ms: Some(wait_ms.max(100)),
                retryable: true,
            }
        } else {
            RateResult {
                granted: false,
                retry_after_ms: None,
                retryable: false,
            }
        }
    }

    /// Get current available tokens
    pub fn available(&mut self) -> u64 {
        self.refill();
        self.tokens as u64
    }

    /// Get total usage by an agent
    pub fn agent_usage(&self, agent_id: &str) -> u64 {
        self.agent_usage.get(agent_id).copied().unwrap_or(0)
    }
}

/// Rate limiter — manages per-provider token buckets
///
/// S4.4.5: Fair scheduling across multiple agents.
/// Uses per-agent usage tracking to prevent any single agent
/// from monopolizing the rate limit.
pub struct RateLimiter {
    /// Per-provider token buckets
    buckets: HashMap<String, TokenBucket>,
    /// Fairness threshold: if an agent has used more than this fraction
    /// of the total capacity, it gets deprioritized.
    fairness_threshold: f64,
}

impl RateLimiter {
    /// Create a new rate limiter
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            fairness_threshold: 0.5,
        }
    }

    /// Add a token bucket for a provider
    pub fn add_bucket(&mut self, provider: &str, capacity: u64, refill_per_sec: f64) {
        self.buckets.insert(
            provider.to_string(),
            TokenBucket::new(provider, capacity, refill_per_sec),
        );
    }

    /// Remove a token bucket for a provider
    pub fn remove_bucket(&mut self, provider: &str) {
        self.buckets.remove(provider);
    }

    /// Try to acquire a rate limit token for a provider
    ///
    /// S4.4.2: RateAcquire handler — token allocation with fair scheduling.
    /// S4.4.5: If the requesting agent has consumed more than the fairness
    /// threshold of total capacity, the request is deprioritized.
    pub fn try_acquire(&mut self, provider: &str) -> RateResult {
        // Without a known agent_id, we can't do fair scheduling.
        // This method is for the simple IPC handler case.
        self.try_acquire_for(provider, "default")
    }

    /// Try to acquire a rate limit token for a specific agent
    pub fn try_acquire_for(&mut self, provider: &str, agent_id: &str) -> RateResult {
        let bucket = match self.buckets.get_mut(provider) {
            Some(b) => b,
            None => {
                // No bucket configured for this provider → always grant
                return RateResult {
                    granted: true,
                    retry_after_ms: None,
                    retryable: true,
                };
            }
        };

        // S4.4.5: Fair scheduling check
        let total_usage: u64 = bucket.agent_usage.values().sum();
        if total_usage > 0 {
            let agent_share = bucket.agent_usage.get(agent_id).copied().unwrap_or(0) as f64
                / total_usage as f64;
            if agent_share > self.fairness_threshold && bucket.tokens < bucket.capacity as f64 * 0.2 {
                // Agent is over its fair share and bucket is running low
                let tokens_needed = 1.0 - bucket.tokens;
                let wait_secs = if bucket.refill_per_sec > 0.0 {
                    tokens_needed / bucket.refill_per_sec
                } else {
                    1.0
                };
                return RateResult {
                    granted: false,
                    retry_after_ms: Some((wait_secs * 1000.0).ceil() as u64 + 500),
                    retryable: true,
                };
            }
        }

        bucket.try_acquire(agent_id)
    }

    /// Check if a provider has rate limiting configured
    pub fn has_bucket(&self, provider: &str) -> bool {
        self.buckets.contains_key(provider)
    }

    /// Get the number of configured buckets
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_bucket_new() {
        let bucket = TokenBucket::new("openai", 100, 10.0);
        assert_eq!(bucket.capacity, 100);
        assert_eq!(bucket.tokens as u64, 100);
        assert!((bucket.refill_per_sec - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_token_bucket_acquire() {
        let mut bucket = TokenBucket::new("openai", 5, 1.0);

        // Should succeed 5 times
        for _ in 0..5 {
            let result = bucket.try_acquire("com.test");
            assert!(result.granted);
        }

        // 6th should fail (retryable)
        let result = bucket.try_acquire("com.test");
        assert!(!result.granted);
        assert!(result.retry_after_ms.is_some());
        assert!(result.retryable);
    }

    #[test]
    fn test_token_bucket_no_refill() {
        let mut bucket = TokenBucket::new("openai", 2, 0.0);

        for _ in 0..2 {
            let result = bucket.try_acquire("com.test");
            assert!(result.granted);
        }

        // S4.4.4: Non-retryable — no refill
        let result = bucket.try_acquire("com.test");
        assert!(!result.granted);
        assert!(result.retry_after_ms.is_none());
        assert!(!result.retryable);
    }

    #[test]
    fn test_token_bucket_acquire_n() {
        let mut bucket = TokenBucket::new("openai", 10, 1.0);

        let result = bucket.try_acquire_n(5, "com.test");
        assert!(result.granted);

        let result = bucket.try_acquire_n(6, "com.test");
        assert!(!result.granted);
        assert!(result.retry_after_ms.is_some());
    }

    #[test]
    fn test_token_bucket_agent_usage() {
        let mut bucket = TokenBucket::new("openai", 10, 1.0);
        bucket.try_acquire("com.test.a");
        bucket.try_acquire("com.test.a");
        bucket.try_acquire("com.test.b");

        assert_eq!(bucket.agent_usage("com.test.a"), 2);
        assert_eq!(bucket.agent_usage("com.test.b"), 1);
        assert_eq!(bucket.agent_usage("com.test.c"), 0);
    }

    #[test]
    fn test_rate_limiter_new() {
        let limiter = RateLimiter::new();
        assert_eq!(limiter.bucket_count(), 0);
    }

    #[test]
    fn test_rate_limiter_add_bucket() {
        let mut limiter = RateLimiter::new();
        limiter.add_bucket("openai", 100, 10.0);
        assert_eq!(limiter.bucket_count(), 1);
        assert!(limiter.has_bucket("openai"));
    }

    #[test]
    fn test_rate_limiter_no_bucket_always_grant() {
        let mut limiter = RateLimiter::new();
        let result = limiter.try_acquire("nonexistent");
        assert!(result.granted);
    }

    #[test]
    fn test_rate_limiter_remove_bucket() {
        let mut limiter = RateLimiter::new();
        limiter.add_bucket("openai", 100, 10.0);
        limiter.remove_bucket("openai");
        assert_eq!(limiter.bucket_count(), 0);
    }

    #[test]
    fn test_rate_limiter_fair_scheduling() {
        let mut limiter = RateLimiter::new();
        limiter.add_bucket("openai", 5, 1.0);

        // Agent A consumes most of the bucket
        for _ in 0..4 {
            let result = limiter.try_acquire_for("openai", "com.agent.a");
            assert!(result.granted);
        }

        // Agent B should still get a token (1 left)
        let result = limiter.try_acquire_for("openai", "com.agent.b");
        assert!(result.granted);

        // Now bucket is empty — both should be denied
        let result = limiter.try_acquire_for("openai", "com.agent.a");
        assert!(!result.granted);
    }
}
