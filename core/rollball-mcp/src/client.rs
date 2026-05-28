// Adapted from zeroclaw/src/tools/mcp_client.rs
// Rollball deviation: uses rollball-mcp's own transport/protocol modules
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

use crate::config::McpServerConfig;
use crate::protocol::{
    JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult,
};
use crate::transport::{McpTransportConn, create_transport};

/// Timeout for receiving a response from an MCP server during init/list.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Default timeout for tool calls (seconds) when not configured per-server.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180;

/// Maximum allowed tool call timeout (seconds) — hard safety ceiling.
const MAX_TOOL_TIMEOUT_SECS: u64 = 600;

// ── Internal server state ──────────────────────────────────────────────────

/// Read-only server metadata (no Mutex needed for reads).
struct McpServerMeta {
    config: McpServerConfig,
    tools: Vec<McpToolDef>,
}

/// Mutable transport state, locked only during IO.
struct McpTransportState {
    /// `None` means the transport has been disconnected / not yet connected.
    transport: Option<Box<dyn McpTransportConn>>,
}

impl McpTransportState {
    /// Replace the transport with a new one, returning the old transport.
    fn replace(&mut self, new: Box<dyn McpTransportConn>) -> Option<Box<dyn McpTransportConn>> {
        std::mem::replace(&mut self.transport, Some(new))
    }
}

// ── McpClient ──────────────────────────────────────────────────────────────

/// A live connection to one MCP server (any transport, e.g. stdio or HTTP).
///
/// After [`McpClient::connect`], the server has been initialized and its
/// tool list has been fetched. Call [`McpClient::call_tool`] for execution.
///
/// If the transport connection is lost (e.g. the MCP server process crashes),
/// `is_alive` is set to `false` and subsequent `call_tool` calls will
/// automatically attempt to reconnect before dispatching the request.
/// Use [`McpClient::reconnect`] explicitly for manual reconnection.
///
/// Internal structure separates read-only metadata from mutable transport
/// state so that `tools()` and `name()` are not blocked by in-flight IO.
pub struct McpClient {
    meta: Arc<McpServerMeta>,
    transport: Arc<Mutex<McpTransportState>>,
    next_id: Arc<AtomicU64>,
    /// Whether the transport is believed to be alive.
    /// Set to `false` on transport errors, set back to `true` on successful reconnect.
    is_alive: Arc<AtomicBool>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            meta: Arc::clone(&self.meta),
            transport: Arc::clone(&self.transport),
            next_id: Arc::clone(&self.next_id),
            is_alive: Arc::clone(&self.is_alive),
        }
    }
}

impl McpClient {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        let mut transport = create_transport(&config).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                config.name
            )
        })?;

        // ── Initialize handshake ──────────────────────────────────────────
        let init_req = JsonRpcRequest::new(
            1u64,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "rollball",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );

        let init_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&init_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out waiting for initialize response",
                config.name
            )
        })??;

        if init_resp.error.is_some() {
            bail!(
                "MCP server `{}` rejected initialize: {:?}",
                config.name,
                init_resp.error
            );
        }

        // Notify server that client is initialized (notification, best-effort)
        let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = transport.send_and_recv(&notif).await;

        // ── Fetch tool list ──────────────────────────────────────────────
        let list_req = JsonRpcRequest::new(2u64, "tools/list", json!({}));

        let list_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&list_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out waiting for tools/list response",
                config.name
            )
        })??;

        let result = list_resp
            .result
            .ok_or_else(|| anyhow!("tools/list returned no result from `{}`", config.name))?;
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        let tool_count = tool_list.tools.len();

        let inner = McpServerMeta {
            config,
            tools: tool_list.tools,
        };

        tracing::info!(
            "MCP server `{}` connected — {} tool(s) available",
            inner.config.name,
            tool_count
        );

        Ok(Self {
            meta: Arc::new(inner),
            transport: Arc::new(Mutex::new(McpTransportState { transport: Some(transport) })),
            next_id: Arc::new(AtomicU64::new(3)), // IDs 1 and 2 were used for init + list
            is_alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Close the connection to this MCP server.
    ///
    /// After calling disconnect, further `call_tool` calls will attempt
    /// auto-reconnection. For stdio transport this shuts down stdin and
    /// kills the child process (via `kill_on_drop`). For HTTP transport
    /// this is a no-op.
    pub async fn disconnect(&self) {
        self.is_alive.store(false, Ordering::SeqCst);
        let mut state = self.transport.lock().await;
        if let Some(mut transport) = state.transport.take() {
            if let Err(e) = transport.close().await {
                tracing::warn!("Error closing MCP server `{}`: {:#}", self.meta.config.name, e);
            }
        }
    }

    /// Whether the transport connection is believed to be alive.
    pub fn is_alive(&self) -> bool {
        self.is_alive.load(Ordering::SeqCst)
    }

    /// Attempt to reconnect to this MCP server.
    ///
    /// Performs a full handshake (initialize + tools/list) with the original
    /// config. On success, `is_alive` is set back to `true` and the transport
    /// is replaced. On failure, the client remains in the disconnected
    /// state and the error is returned.
    pub async fn reconnect(&self) -> Result<()> {
        let config = &self.meta.config;
        tracing::info!("MCP server `{}`: attempting reconnect", config.name);

        // Perform a full connect (handshake + tools/list) to get a fresh client
        let new_client = Self::connect(config.clone()).await?;

        // Replace the transport under the lock.
        // The old transport (if any) is closed (best-effort) and dropped.
        {
            let mut new_state = new_client.transport.lock().await;
            let mut state = self.transport.lock().await;
            // New client always has Some(transport) after successful connect
            let new_transport = new_state.transport.take()
                .expect("new client must have a transport after connect");
            if let Some(mut old) = state.replace(new_transport) {
                let _ = old.close().await;
            }
        }

        // Reset the request ID counter to align with the new connection
        let new_next_id = new_client.next_id.load(Ordering::Relaxed);
        self.next_id.store(new_next_id, Ordering::Relaxed);

        self.is_alive.store(true, Ordering::SeqCst);
        tracing::info!("MCP server `{}`: reconnected successfully", config.name);
        Ok(())
    }

    /// Tools advertised by this server.
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.meta.tools.clone()
    }

    /// Server display name.
    pub fn name(&self) -> String {
        self.meta.config.name.clone()
    }

    /// Call a tool on this server. Returns the raw JSON result.
    ///
    /// Only the transport Mutex is held during IO — metadata reads
    /// (`tools()`, `name()`) are never blocked by in-flight calls.
    ///
    /// If the transport is disconnected, returns an error without attempting
    /// auto-reconnect (the caller should use `reconnect()` explicitly or via
    /// the registry's `call_tool` which handles auto-reconnect).
    /// Transport errors mark this client as not alive.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(
            id,
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
        );

        let tool_timeout = self
            .meta
            .config
            .tool_timeout_secs
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
            .min(MAX_TOOL_TIMEOUT_SECS);

        let mut state = self.transport.lock().await;
        let transport = match state.transport.as_mut() {
            Some(t) => t,
            None => {
                self.is_alive.store(false, Ordering::SeqCst);
                bail!(
                    "MCP server `{}` is disconnected — transport not available",
                    self.meta.config.name
                );
            }
        };

        let result = timeout(
            Duration::from_secs(tool_timeout),
            transport.send_and_recv(&req),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.error {
                    bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
                }
                Ok(resp.result.unwrap_or(serde_json::Value::Null))
            }
            Ok(Err(e)) => {
                // Transport-level error — mark as not alive
                self.is_alive.store(false, Ordering::SeqCst);
                Err(e).with_context(|| {
                    format!(
                        "MCP server `{}` error during tool call `{tool_name}`",
                        self.meta.config.name
                    )
                })
            }
            Err(_) => {
                // Timeout — don't mark as not alive (server may just be slow)
                Err(anyhow!(
                    "MCP server `{}` timed out after {}s during tool call `{tool_name}`",
                    self.meta.config.name,
                    tool_timeout
                ))
            }
        }
    }
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
///
/// Tools are indexed by a prefixed name (`mcp:<server_name>__<tool_name>`) to
/// prevent name collisions across servers.
pub struct McpRegistry {
    servers: Vec<McpClient>,
    /// prefixed_name -> (server_index, McpToolDef)
    /// The tool def is stored directly in the index to avoid a secondary
    /// linear search in `McpServerMeta.tools`.
    tool_index: HashMap<String, (usize, McpToolDef)>,
}

impl McpRegistry {
    /// Connect to all configured servers in parallel. Non-fatal: failures are logged and skipped.
    ///
    /// All server connections are attempted concurrently via `join_all`,
    /// reducing total startup time from `N × timeout` to a single timeout
    /// window when multiple servers are configured.
    pub async fn connect_all(configs: &[McpServerConfig]) -> Result<Self> {
        if configs.is_empty() {
            return Ok(Self {
                servers: Vec::new(),
                tool_index: HashMap::new(),
            });
        }

        // Connect to all servers in parallel
        let connect_futures: Vec<_> = configs
            .iter()
            .map(|config| async {
                let name = config.name.clone();
                let result = McpClient::connect(config.clone()).await;
                (name, result)
            })
            .collect();

        let results = join_all(connect_futures).await;

        // Build server list and tool index from successful connections
        let mut servers = Vec::new();
        let mut tool_index = HashMap::new();

        for (name, result) in results {
            match result {
                Ok(server) => {
                    let server_idx = servers.len();
                    let tools = server.tools();
                    for tool in tools {
                        let prefixed = format!("mcp:{}__{}", name, tool.name);
                        tool_index.insert(prefixed, (server_idx, tool));
                    }
                    servers.push(server);
                }
                Err(e) => {
                    tracing::error!("Failed to connect to MCP server `{}`: {:#}", name, e);
                }
            }
        }

        Ok(Self {
            servers,
            tool_index,
        })
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    /// Single HashMap lookup — no secondary linear search.
    pub fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (_, def) = self.tool_index.get(prefixed_name)?;
        Some(def.clone())
    }

    /// Execute a tool by prefixed name.
    ///
    /// If the target server is not alive (transport broken), attempts an
    /// automatic reconnection before dispatching the call. If reconnection
    /// fails, returns the reconnection error.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let (server_idx, def) = self
            .tool_index
            .get(prefixed_name)
            .ok_or_else(|| anyhow!("unknown MCP tool `{prefixed_name}`"))?;

        let server = &self.servers[*server_idx];
        let original_name = &def.name;

        // Auto-reconnect: if the server's transport is dead, try once
        if !server.is_alive() {
            tracing::info!(
                "MCP server `{}` is not alive, attempting auto-reconnect for tool `{}`",
                server.name(),
                prefixed_name,
            );
            server.reconnect().await?;
        }

        let result = server.call_tool(original_name, arguments).await?;
        serde_json::to_string_pretty(&result)
            .with_context(|| format!("failed to serialize result of MCP tool `{prefixed_name}`"))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// Close connections to all MCP servers.
    ///
    /// Individual close errors are logged and skipped. After calling
    /// disconnect, further `call_tool` calls will return errors.
    pub async fn disconnect(&self) {
        for server in &self.servers {
            server.disconnect().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_prefix_format() {
        let prefixed = format!("mcp:{}__{}", "filesystem", "read_file");
        assert_eq!(prefixed, "mcp:filesystem__read_file");
    }

    #[tokio::test]
    async fn connect_nonexistent_command_fails_cleanly() {
        let config = McpServerConfig {
            name: "nonexistent".to_string(),
            command: "/usr/bin/this_binary_does_not_exist_rollball_test".to_string(),
            ..Default::default()
        };
        let result = McpClient::connect(config).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("failed to create transport"), "got: {msg}");
    }

    #[tokio::test]
    async fn connect_all_nonfatal_on_single_failure() {
        let configs = vec![McpServerConfig {
            name: "bad".to_string(),
            command: "/usr/bin/does_not_exist_rb_test".to_string(),
            ..Default::default()
        }];
        let registry = McpRegistry::connect_all(&configs)
            .await
            .expect("connect_all should not fail");
        assert!(registry.is_empty());
        assert_eq!(registry.tool_count(), 0);
    }

    #[tokio::test]
    async fn empty_registry_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all on empty slice should succeed");
        assert!(registry.is_empty());
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
    }

    #[tokio::test]
    async fn empty_registry_tool_names_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn empty_registry_get_tool_def_returns_none() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let result = registry.get_tool_def("mcp:nonexistent__tool");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_registry_call_tool_unknown_name_returns_error() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let err = registry
            .call_tool("mcp:nonexistent__tool", serde_json::json!({}))
            .await
            .expect_err("should fail for unknown tool");
        assert!(err.to_string().contains("unknown MCP tool"), "got: {err}");
    }
}
