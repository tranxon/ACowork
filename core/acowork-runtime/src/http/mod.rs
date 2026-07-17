//! HTTP module — Runtime localhost HTTP server (ADR-033 Phase 2).
//!
//! The Runtime starts a localhost-only HTTP server on a random port
//! (`127.0.0.1:0`). The Gateway reverse-proxies large data queries
//! (session lists, message lists, memory graph, file content) to this
//! server rather than reading Runtime local files directly.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.

pub mod memory_query;
pub mod server;

pub use server::{
    RuntimeHttpServer, RuntimeHttpServerError, SharedDegradation, SharedDispatchSender,
    SharedEmbedDimension,
    SharedMemoryStore,
};
