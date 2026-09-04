//! RuntimeAgentToolsService — implements [`AgentToolsService`].
//!
//! ADR-040 follow-up: the Tools-panel persistence endpoints
//! (`/agents/{id}/mcp-servers`, `/agents/{id}/search-config`,
//! `/agents/{id}/builtin-tools`) used to call `agent_config::*` directly
//! from the HTTP handlers, bypassing the UseCase trait layer. This
//! module consolidates those operations behind a single struct so the
//! HTTP handlers become thin protocol converters — consistent with
//! `RuntimeWorkspaceMutationService` and the rest of the layer.
//!
//! ## State
//!
//! Holds the agent `work_dir` resolved at boot (no async resource
//! dependencies), so the service can be constructed immediately after
//! the workspace services in `session_init.rs` Phase B and doesn't
//! require a late-bind slot's typical "wait for Phase B" setup. We
//! still publish it through a slot so future post-Phase-A consumers
//! (e.g. desktop snapshot streaming) can grab it.
//!
//! ## File layout
//!
//! All disk I/O goes through [`crate::agent_config`] module functions
//! (`load_agent_mcp_config` / `save_agent_mcp_config`,
//! `load_agent_search_config` / `save_agent_search_config`, and
//! `load_agent_tools_config` / `save_agent_tools_config` for
//! builtin tools). This service is the **single audit point** for the
//! six endpoints — path construction, atomic write-tmp-rename, the
//! `active_names` validation rule for MCP, and the
//! read-modify-write cycle for builtin tools all live here, not in
//! the handlers.
//!
//! ## Validation
//!
//! - `put_mcp_servers` validates every requested name against
//!   `AgentMcpConfig::merged()` (catalog + local) and rejects the
//!   whole batch if any name is unknown. Atomic semantics, no partial
//!   writes.
//! - `put_builtin_tools` does **not** reject unknown tool names — it
//!   silently drops them per ADR-029 §7. The rationale: registered
//!   builtin tools evolve across releases, so the desktop's local
//!   cache can carry names that no longer exist; rejecting them would
//!   force a hard desync. The same `apply_builtin_tools_patch` helper
//!   used by the MQTT `RuntimeConfigUpdate` path runs here for
//!   identical semantics.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;
use acowork_core::protocol::AgentSearchConfig;

use crate::agent_config;
use crate::usecases::agent_tools::{
    AgentToolsError, AgentToolsService, BuiltinToolsResponse, McpServersResponse,
    McpToolsResponse, MergedToolsResponse, PutBuiltinToolsBody, PutMcpServersBody,
    PutMcpToolsBody, PutSearchConfigBody, SearchConfigResponse,
};

/// Concrete [`AgentToolsService`] backed by the
/// `workspace/config/agent_mcp.json`, `agent_search.json`, and
/// `agent_tools.json` files.
pub struct RuntimeAgentToolsService {
    work_dir: PathBuf,
}

impl RuntimeAgentToolsService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }
}

#[async_trait]
impl AgentToolsService for RuntimeAgentToolsService {
    async fn get_mcp_servers(&self, agent_id: &str) -> McpServersResponse {
        let active = agent_config::load_agent_mcp_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|m| m.active_server_names())
            .unwrap_or_default();
        McpServersResponse {
            agent_id: agent_id.to_string(),
            active_servers: active,
        }
    }

    async fn put_mcp_servers(
        &self,
        agent_id: &str,
        body: PutMcpServersBody,
    ) -> Result<McpServersResponse, AgentToolsError> {
        // 1. Load current config (catalog + local + prior active_names).
        let mut cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .map_err(AgentToolsError::Persistence)?
            .unwrap_or_default();

        // 2. Validate every requested name against the merged set
        //    (catalog ∪ local). Empty body is allowed and means
        //    "explicitly no servers active" (Some(vec![])).
        if !body.servers.is_empty() {
            let merged_names: Vec<String> = cfg
                .merged()
                .into_iter()
                .map(|s| s.name)
                .collect();
            let merged: HashSet<&str> =
                merged_names.iter().map(|s| s.as_str()).collect();
            let unknown: Vec<String> = body
                .servers
                .iter()
                .filter(|name| !merged.contains(name.as_str()))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                return Err(AgentToolsError::UnknownServers(unknown));
            }
        }

        // 3. Atomic read-modify-write: replace active_names with the
        //    user's full selection. Catalog + local are preserved on
        //    disk (write-back via save_agent_mcp_config).
        cfg.active_names = Some(body.servers.clone());
        agent_config::save_agent_mcp_config(&self.work_dir, &cfg)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            active_count = body.servers.len(),
            "RuntimeAgentToolsService::put_mcp_servers: active_names persisted"
        );

        Ok(McpServersResponse {
            agent_id: agent_id.to_string(),
            active_servers: body.servers,
        })
    }

    async fn get_search_config(&self, agent_id: &str) -> SearchConfigResponse {
        let providers = agent_config::load_agent_search_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|c| c.providers)
            .unwrap_or_default();
        SearchConfigResponse {
            agent_id: agent_id.to_string(),
            providers,
        }
    }

    async fn put_search_config(
        &self,
        agent_id: &str,
        body: PutSearchConfigBody,
    ) -> Result<SearchConfigResponse, AgentToolsError> {
        let current = agent_config::load_agent_search_config(&self.work_dir)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to load agent_search.json, using default");
                None
            })
            .unwrap_or_default();
        let cfg = AgentSearchConfig {
            providers: body.providers.clone(),
            catalog: current.catalog,
        };
        agent_config::save_agent_search_config(&self.work_dir, &cfg)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            provider_count = body.providers.len(),
            "RuntimeAgentToolsService::put_search_config: providers persisted"
        );

        Ok(SearchConfigResponse {
            agent_id: agent_id.to_string(),
            providers: body.providers,
        })
    }

    async fn get_builtin_tools(&self, agent_id: &str) -> BuiltinToolsResponse {
        let tools = agent_config::load_agent_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|c| c.tools)
            .unwrap_or_default();
        BuiltinToolsResponse {
            agent_id: agent_id.to_string(),
            tools,
        }
    }

    async fn put_builtin_tools(
        &self,
        agent_id: &str,
        body: PutBuiltinToolsBody,
    ) -> Result<BuiltinToolsResponse, AgentToolsError> {
        // Read-modify-write cycle (matches the previous handler logic
        // and the MQTT `RuntimeConfigUpdate` path in `cli.rs`):
        //
        //   1. Load the current `agent_tools.json` (or default empty if
        //      no file yet — first-run case after a fresh agent install).
        //   2. Build a patch by iterating the **current** entries and
        //      setting each entry's `enabled` based on whether its name
        //      appears in `body.builtin_tools`.  This iteration is the
        //      critical detail that prevents the "every unchecked
        //      checkbox silently re-enables on next PUT" bug — without
        //      it, `apply_builtin_tools_patch` would only flip tools
        //      named in the patch and leave everything else alone.
        //   3. Apply the patch (which force-enables PLATFORM_TOOLS and
        //      silently ignores names not in current).
        //   4. Persist via `save_agent_tools_config` (atomic
        //      write-tmp-rename).
        let current = agent_config::load_agent_tools_config(&self.work_dir)
            .map_err(AgentToolsError::Persistence)?
            .map(|c| c.tools)
            .unwrap_or_default();
        let patch: Vec<agent_config::AgentToolEntry> = current
            .iter()
            .map(|entry| {
                let enabled = body.builtin_tools.iter().any(|n| n == &entry.name);
                agent_config::AgentToolEntry::new(&entry.name, enabled)
            })
            .collect();
        let updated = agent_config::apply_builtin_tools_patch(&current, &patch);
        agent_config::save_agent_tools_config(
            &self.work_dir,
            &agent_config::AgentToolsConfig {
                tools: updated.clone(),
            },
        )
        .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            enabled_count = updated.iter().filter(|e| e.enabled).count(),
            total = updated.len(),
            "RuntimeAgentToolsService::put_builtin_tools: enabled flags persisted"
        );

        Ok(BuiltinToolsResponse {
            agent_id: agent_id.to_string(),
            tools: updated,
        })
    }

    async fn get_merged_tools(&self, agent_id: &str) -> MergedToolsResponse {
        let tools = agent_config::load_agent_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|t| t.tools)
            .unwrap_or_default();

        let mcp_cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .ok()
            .flatten();

        let mcp_servers = mcp_cfg
            .as_ref()
            .map(|m| m.active_server_names())
            .unwrap_or_default();

        // Full per-agent merged list (catalog ∪ local) with active flags.
        // This powers the per-agent switches in the Desktop Tools panel so
        // agent-installed (local) and system-injected (pm) servers appear
        // there too — not just gateway-global catalog entries.
        let active_set: std::collections::HashSet<&str> =
            mcp_servers.iter().map(String::as_str).collect();
        let mcp_servers_defs = mcp_cfg
            .as_ref()
            .map(|m| {
                m.merged()
                    .into_iter()
                    .map(|s| crate::usecases::agent_tools::McpServerView {
                        name: s.name.clone(),
                        transport: s.transport,
                        url: s.url.clone(),
                        command: s.command.clone(),
                        args: s.args.clone(),
                        active: active_set.contains(s.name.as_str()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let search = agent_config::load_agent_search_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|cfg| serde_json::json!({ "providers": cfg.providers }))
            .unwrap_or_else(|| serde_json::json!({ "providers": [] }));

        MergedToolsResponse {
            agent_id: agent_id.to_string(),
            tools,
            mcp_servers,
            mcp_servers_defs,
            search,
        }
    }

    async fn get_mcp_tools(&self, agent_id: &str) -> McpToolsResponse {
        let servers = agent_config::load_agent_mcp_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|cfg| cfg.servers)
            .unwrap_or_default();
        McpToolsResponse {
            agent_id: agent_id.to_string(),
            servers,
        }
    }

    async fn put_mcp_tools(
        &self,
        agent_id: &str,
        body: PutMcpToolsBody,
    ) -> Result<McpToolsResponse, AgentToolsError> {
        // The desktop PUTs the entire per-server tool list (same shape
        // as GET). There is no server-side merge — the desktop's list
        // is authoritative. Write verbatim to agent_mcp_tools.json.
        agent_config::save_agent_mcp_tools_config(&self.work_dir, &body)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            server_count = body.servers.len(),
            tool_count = body.servers.values().map(|v| v.len()).sum::<usize>(),
            "RuntimeAgentToolsService::put_mcp_tools: persisted full tool list"
        );

        Ok(McpToolsResponse {
            agent_id: agent_id.to_string(),
            servers: body.servers,
        })
    }
}