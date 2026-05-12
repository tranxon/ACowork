//! Debug protocol module for Agent Runtime DevMode.
//!
//! Provides:
//! - [`protocol`]: JSON-RPC 2.0 message types (request, response, notification)
//! - [`controller`]: DebugController — shared state (execution control, breakpoints, snapshots)
//! - [`server`]: WebSocket server (ws://127.0.0.1:19878) for Desktop App communication
//!
//! The debug protocol follows Chrome DevTools Protocol (CDP) conventions
//! with JSON-RPC 2.0 over WebSocket. See `docs/design/10-debug-protocol.md`.

pub mod controller;
pub mod protocol;
pub mod server;
