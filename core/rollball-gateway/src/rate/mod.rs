//! Rate limiting module
//!
//! Per-provider token bucket rate limiting with configurable
//! refill rates, burst sizes, and fair scheduling across agents.

pub mod bucket;

pub use bucket::{RateLimiter, RateResult, TokenBucket};
