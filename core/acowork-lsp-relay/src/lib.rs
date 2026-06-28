//! acowork-lsp-relay — Standalone LSP protocol relay process.
//!
//! Managed by the Gateway supervisor (spawn / monitor / restart).
//! Provides WebSocket LSP relay and JSON-RPC API for codebase tools.

pub mod codebase;
pub mod config;
pub mod install;
pub mod pool;
pub mod protocol;
pub mod relay;
pub mod server;
pub mod state;
