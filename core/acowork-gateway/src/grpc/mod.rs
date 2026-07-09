//! gRPC server module.
//!
//! Provides a tonic-based bidirectional streaming server as the sole
//! transport for Runtime ↔ Gateway communication. This module reuses the same
//! business logic (handler functions) as the gRPC dispatcher, converting
//! between proto types and domain types.

pub mod dispatch;
pub mod resource_pusher;
pub mod server;

// Re-export the main entry point and types
pub use server::SharedGrpcSessionMgr;
pub use server::start_grpc_server;
