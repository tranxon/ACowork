//! Gateway handler module
//!
//! Contains handler functions for Gateway Service API requests
//! and session management for connected Agent Runtimes.

pub mod server;
pub mod session;
pub mod global_push;

// Re-export SharedState for convenience
pub use server::SharedState;
