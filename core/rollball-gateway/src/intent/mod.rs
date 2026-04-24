//! Intent routing module
//!
//! Routes Intent messages between Agents and applies privacy filters
//! to responses before cross-agent forwarding.

pub mod privacy;
pub mod router;

pub use router::{IntentRouter, IntentError, IntentResult, DEFAULT_INTENT_TIMEOUT_SECS};
