//! MCP (Model Context Protocol) manager — connection lifecycle and tool injection.
//!
//! Manages MCP server connections and provides [`McpToolWrapper`] instances
//! that implement the built-in [`Tool`](acowork_core::tools::traits::Tool) trait,
//! enabling MCP tools to be dispatched transparently alongside native AgentCowork tools.

use std::sync::Arc;

use acowork_core::protocol::McpServerConfigDef;
use acowork_core::tools::traits::Tool;
use acowork_mcp::client::McpRegistry;
use acowork_mcp::wrapper::McpToolWrapper;

/// Re-export from acowork-mcp so SessionManager can reference it.
pub use acowork_mcp::client::McpConnectionFailure;

/// Result of an asynchronous MCP server connection attempt.
///
/// Produced by a background task and applied to SessionManager
/// via [`SessionManager::apply_mcp_connection_result`].
pub type McpConnectResult = (
    Arc<McpRegistry>,
    Vec<McpToolWrapper>,
    Vec<(String, serde_json::Value)>,
    Vec<McpConnectionFailure>,
);

/// MCP connection manager.
///
/// Holds a shared [`McpRegistry`] and provides helpers for connecting
/// servers and building tool wrappers.
pub struct McpManager {
    registry: Option<Arc<McpRegistry>>,
}

impl McpManager {
    /// Create an empty MCP manager (no servers connected).
    pub fn new() -> Self {
        Self { registry: None }
    }

    /// Connect to MCP servers and create tool wrappers.
    ///
    /// - `configs`: list of MCP server configurations.
    ///
    /// Returns a tuple of:
    ///   - `Arc<McpRegistry>` — shared registry for tool dispatch
    ///   - `Vec<McpToolWrapper>` — one wrapper per MCP tool
    ///   - `Vec<(String, serde_json::Value)>` — tool specs for LLM definitions
    ///   - `Vec<McpConnectionFailure>` — connection failures to surface to LLM
    ///
    /// On connection failure, individual servers are skipped (logged as errors).
    /// The returned registry may be empty if no servers connected successfully.
    pub async fn connect(
        &mut self,
        configs: &[McpServerConfigDef],
    ) -> (
        Arc<McpRegistry>,
        Vec<McpToolWrapper>,
        Vec<(String, serde_json::Value)>,
        Vec<McpConnectionFailure>,
    ) {
        // McpServerConfigDef is now the single source of truth for MCP config,
        // shared between acowork-core (wire format) and acowork-mcp (runtime).
        // No conversion needed — the same type flows through both crates.
        let (registry, failures) = McpRegistry::connect_all(configs)
            .await
            .expect("connect_all is non-fatal and should never fail");
        let registry = Arc::new(registry);

        // Build tool wrappers and specs from the registry
        let mut wrappers = Vec::new();
        let mut specs = Vec::new();

        for prefixed_name in registry.tool_names() {
            let prefixed = prefixed_name.clone();
            if let Some(def) = registry.get_tool_def(&prefixed) {
                let wrapper = McpToolWrapper::new(prefixed.clone(), def, registry.clone());
                let spec = wrapper.spec();
                let serialized = serde_json::to_value(&spec).unwrap_or_default();
                specs.push((spec.name.clone(), serialized));
                wrappers.push(wrapper);
            }
        }

        tracing::info!(
            server_count = registry.server_count(),
            tool_count = wrappers.len(),
            failure_count = failures.len(),
            "MCP manager: connected"
        );

        self.registry = Some(registry.clone());
        (registry, wrappers, specs, failures)
    }

    /// Get the current MCP registry, if any servers are connected.
    pub fn registry(&self) -> Option<&Arc<McpRegistry>> {
        self.registry.as_ref()
    }

    /// Check whether any MCP servers are connected.
    pub fn is_connected(&self) -> bool {
        self.registry.as_ref().is_some_and(|r| !r.is_empty())
    }

    /// Set the registry directly (used when MCP connection results are
    /// produced by a background task and applied asynchronously).
    pub fn set_registry(&mut self, registry: Arc<McpRegistry>) {
        self.registry = Some(registry);
    }

    /// Disconnect from all MCP servers and release resources.
    ///
    /// Closes transport connections (kills stdio child processes, releases
    /// HTTP connection pools). After calling disconnect, the manager is
    /// reset to the empty state and `connect()` must be called again before
    /// using MCP tools.
    pub async fn disconnect(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.disconnect().await;
            tracing::info!("MCP manager: disconnected from all servers");
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::McpTransportDef;

    #[test]
    fn mcp_manager_default_is_not_connected() {
        let mgr = McpManager::default();
        assert!(!mgr.is_connected());
        assert!(mgr.registry().is_none());
    }

    #[tokio::test]
    async fn connect_empty_yields_empty_registry() {
        let mut mgr = McpManager::new();
        let (registry, wrappers, specs, failures) = mgr.connect(&[]).await;
        assert!(registry.is_empty());
        assert!(wrappers.is_empty());
        assert!(specs.is_empty());
        assert!(failures.is_empty());
        assert!(!mgr.is_connected());
    }

    #[test]
    fn config_def_is_shared_type() {
        // McpServerConfigDef is now used directly by acowork-mcp,
        // no separate conversion step needed.
        let def = McpServerConfigDef {
            name: "test-server".to_string(),
            transport: McpTransportDef::Stdio,
            url: None,
            command: "test-cmd".to_string(),
            args: vec!["--verbose".to_string()],
            env: Default::default(),
            headers: Default::default(),
            tool_timeout_secs: Some(30),
        };
        assert_eq!(def.name, "test-server");
        assert_eq!(def.command, "test-cmd");
        assert_eq!(def.args, vec!["--verbose"]);
        assert_eq!(def.tool_timeout_secs, Some(30));
        assert!(matches!(def.transport, McpTransportDef::Stdio));
        assert!(def.url.is_none());
    }
}
