//! rollball-mcp — MCP (Model Context Protocol) client library.
//!
//! Provides protocol types, transport abstraction, and a client for connecting
//! to MCP tool servers. Adapted from zeroclaw's MCP implementation.
//! SPDX-License-Identifier: MIT OR Apache-2.0

pub mod config;
pub mod client;
pub mod protocol;
pub mod transport;
pub mod wrapper;

pub use config::{McpConfig, McpServerConfig, McpTransport};
pub use client::{McpClient, McpRegistry};
pub use protocol::{JsonRpcRequest, JsonRpcResponse, McpToolDef, MCP_PROTOCOL_VERSION};
pub use transport::{McpTransportConn, create_transport};
pub use wrapper::McpToolWrapper;
