//! gRPC server module for Gateway IPC.
//!
//! Provides a tonic-based bidirectional streaming server as an alternative
//! to the custom-framing IPC transport. This module reuses the same
//! business logic (handler functions) as the IPC server, but converts
//! between proto types and domain types instead of JSON framing.

pub mod server;
pub mod dispatch;

// Re-export the main entry point
pub use server::start_grpc_server;
