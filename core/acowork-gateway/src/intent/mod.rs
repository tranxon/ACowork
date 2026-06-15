//! Intent routing module
//!
//! Routes Intent messages between Agents and applies privacy filters
//! to responses before cross-agent forwarding.

pub mod privacy;
pub mod router;

pub use router::{DEFAULT_INTENT_TIMEOUT_SECS, IntentError, IntentResult, IntentRouter};
