//! Agent tools configuration use case (Tools panel selection persistence).
//!
//! ADR-040 follow-up: the Tools-panel persistence HTTP handlers (added
//! for the Desktop Tools panel persistence fix) used to call
//! `agent_config::*` directly, bypassing the project's UseCase trait
//! layer. This module exposes those operations behind
//! [`AgentToolsService`] so HTTP handlers become thin protocol
//! converters — consistent with
//! [`crate::usecases::WorkspaceMutationService`],
//! [`crate::usecases::MemoryQueryService`], and the rest of the layer.
//!
//! ## Scope
//!
//! The trait covers **all persistence operations on the three
//! per-agent `agent_*` JSON files that the Tools panel mutates**:
//!
//! | HTTP endpoint | Trait method | File on disk |
//! |---|---|---|
//! | `GET  /agents/{id}/mcp-servers`   | [`AgentToolsService::get_mcp_servers`]   | `agent_mcp.json` |
//! | `PUT  /agents/{id}/mcp-servers`   | [`AgentToolsService::put_mcp_servers`]   | `agent_mcp.json` |
//! | `GET  /agents/{id}/search-config` | [`AgentToolsService::get_search_config`] | `agent_search.json` |
//! | `PUT  /agents/{id}/search-config` | [`AgentToolsService::put_search_config`] | `agent_search.json` |
//! | `GET  /agents/{id}/builtin-tools` | [`AgentToolsService::get_builtin_tools`] | `agent_tools.json` |
//! | `PUT  /agents/{id}/builtin-tools` | [`AgentToolsService::put_builtin_tools`] | `agent_tools.json` |
//!
//! **Out of scope (different files, separate trait):**
//!
//! - `GET/PUT /agents/{id}/config` — mutates `agent_config.json` (the
//!   per-agent **runtime** config: temperature, context_window,
//!   max_iterations, …). Lives behind
//!   [`crate::usecases::AgentConfigService`] because that file has a
//!   separate read-modify-write contract, MQTT re-PUBLISH semantics,
//!   and live-broadcast side effects (none of which apply to the
//!   `agent_tools.json` / `agent_mcp.json` / `agent_search.json` files).
//! - `GET /agents/{id}/tools` — read-only merge of the three files,
//!   frontend convenience. Trivially inlinable; not worth a trait method.
//!
//! ## Errors
//!
//! Methods that mutate disk state return
//! [`AgentToolsError::Persistence`] for any I/O or JSON failure — the
//! HTTP layer maps that to 500. Method
//! [`AgentToolsService::put_mcp_servers`] additionally returns
//! [`AgentToolsError::UnknownServers`] (HTTP 400) when a submitted
//! server name does not exist in the merged catalog (catalog + local).
//! `put_builtin_tools` deliberately **does not** validate unknown tool
//! names — it silently ignores them per ADR-029 §7 (the registered
//! tool set evolves across releases, so the front-end's local cache
//! can be stale; see `apply_builtin_tools_patch` for the merge rule).
//!
//! ## Late-bind wiring
//!
//! The implementation [`crate::usecases::RuntimeAgentToolsService`]
//! holds the `work_dir` resolved at boot (no async resource
//! dependencies), so it can be constructed immediately after the
//! workspace services in `session_init.rs` Phase B.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// All error variants that MCP / search config operations can produce.
///
/// The HTTP layer maps each variant to a deterministic status code so
/// the desktop can distinguish "unknown name" (400) from a generic
/// persistence failure (500) without parsing error strings.
#[derive(Debug, thiserror::Error)]
pub enum AgentToolsError {
    /// The requested MCP server name does not exist in the merged
    /// catalog (catalog entries ∪ local user-installed servers).
    /// Maps to HTTP 400 — the desktop already filters to catalog items,
    /// so this protects against stale names reaching us via direct API
    /// calls.
    #[error("unknown MCP server names (not in catalog+local): {0:?}")]
    UnknownServers(Vec<String>),

    /// Failed to load, parse, or persist an `agent_mcp.json` /
    /// `agent_search.json` file. Maps to HTTP 500.
    #[error("failed to persist agent tools config: {0}")]
    Persistence(String),
}

// ── Request / Response DTOs ─────────────────────────────────────────────

/// Request body for `PUT /agents/{id}/mcp-servers`.
///
/// Mirrors the desktop `mcpStore.setActiveServers({servers: [...]})`
/// wire shape (camelCase on the wire via the existing handler).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutMcpServersBody {
    /// Catalog names the user ticked in the Tools panel. Each must
    /// resolve to a server in `cfg.merged()`.
    #[serde(default)]
    pub servers: Vec<String>,
}

/// Request body for `PUT /agents/{id}/builtin-tools`.
///
/// Wire shape matches `ToolsTab.toggleBuiltinTool` in the desktop:
/// `{ builtin_tools: [...] }` — the **complete** enabled set. The
/// runtime applies a read-modify-write cycle against `agent_tools.json`
/// so unlisted tools are flipped to `enabled = false`. Platform tools
/// (see `PLATFORM_TOOLS`) are force-enabled by the patcher.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutBuiltinToolsBody {
    /// Names of builtin tools the user wants enabled.
    #[serde(default)]
    pub builtin_tools: Vec<String>,
}

/// Request body for `PUT /agents/{id}/search-config`.
///
/// Wire shape matches `acowork_core::protocol::AgentSearchProvider` 1:1
/// so the Gateway proxy pass-through is transparent. Each entry carries
/// the provider id and its priority (1 = highest priority, lower
/// number = tried first in the fallback chain).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutSearchConfigBody {
    /// Ordered list of active search providers for this agent.
    #[serde(default)]
    pub providers: Vec<acowork_core::protocol::AgentSearchProvider>,
}

/// Response for `GET /agents/{id}/mcp-servers`.
#[derive(Debug, Clone, Serialize)]
pub struct McpServersResponse {
    /// Echo of the requested `agent_id` for client-side routing.
    pub agent_id: String,
    /// Active server names (honors `active_names` if set on
    /// `AgentMcpConfig`; falls back to the merged catalog list when the
    /// user has never explicitly chosen).
    pub active_servers: Vec<String>,
}

/// Response for `GET /agents/{id}/search-config`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchConfigResponse {
    /// Echo of the requested `agent_id` for client-side routing.
    pub agent_id: String,
    /// Ordered list of active search providers with priority.
    /// Empty list means "no providers selected" — distinct from
    /// "config file missing" which is a 500.
    pub providers: Vec<acowork_core::protocol::AgentSearchProvider>,
}

/// Response for `GET /agents/{id}/builtin-tools` (and the `PUT`
/// acknowledgement). Mirrors the `agent_tools.json` shape so the
/// frontend can re-render without a second round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct BuiltinToolsResponse {
    /// Echo of the requested `agent_id` for client-side routing.
    pub agent_id: String,
    /// Per-tool entries with their enabled flag — same shape as
    /// [`crate::agent_config::AgentToolsConfig::tools`].
    pub tools: Vec<crate::agent_config::AgentToolEntry>,
}

/// Response for `GET /agents/{id}/tools` - the merged Tools-panel view.
///
/// Combines all three Tools-panel sources (builtin tools, MCP servers,
/// search providers) in a single round-trip so the desktop can render
/// the entire panel without chaining three separate calls.
#[derive(Debug, Clone, Serialize)]
pub struct MergedToolsResponse {
    pub agent_id: String,
    pub tools: Vec<crate::agent_config::AgentToolEntry>,
    pub mcp_servers: Vec<String>,
    pub search: serde_json::Value,
}

// ── Trait ──────────────────────────────────────────────────────────────

/// UseCase trait for the Tools-panel persistence endpoints that mutate
/// the three per-agent `agent_*` JSON files the Tools panel owns
/// (`agent_mcp.json`, `agent_search.json`, `agent_tools.json`).
///
/// See the module-level docs for the rationale, the explicit list of
/// covered endpoints, and the pre-existing handlers that are
/// intentionally out of scope (notably `PUT /agents/{id}/config`,
/// which belongs to `agent_config.json` and lives behind
/// [`crate::usecases::AgentConfigService`]).
#[async_trait]
pub trait AgentToolsService: Send + Sync {
    /// `GET /agents/{id}/mcp-servers` — list active MCP server names.
    async fn get_mcp_servers(&self, agent_id: &str) -> McpServersResponse;

    /// `PUT /agents/{id}/mcp-servers` — persist the active MCP server
    /// selection. Each name in `body.servers` must resolve in
    /// `cfg.merged()`; otherwise the whole request fails with
    /// [`AgentToolsError::UnknownServers`] (no partial writes).
    async fn put_mcp_servers(
        &self,
        agent_id: &str,
        body: PutMcpServersBody,
    ) -> Result<McpServersResponse, AgentToolsError>;

    /// `GET /agents/{id}/search-config` — list active search provider IDs.
    async fn get_search_config(&self, agent_id: &str) -> SearchConfigResponse;

    /// `PUT /agents/{id}/search-config` — persist the active search
    /// provider selection. The body replaces the full `providers` list
    /// (no per-field merge — matches the desktop's "this is the
    /// complete enabled set" semantic, parallel to
    /// `put_builtin_tools`'s read-modify-write cycle).
    async fn put_search_config(
        &self,
        agent_id: &str,
        body: PutSearchConfigBody,
    ) -> Result<SearchConfigResponse, AgentToolsError>;

    /// `GET /agents/{id}/builtin-tools` — list all builtin tools with
    /// their enabled flag. Returns an empty `tools` vec when no
    /// `agent_tools.json` exists on disk (distinct from a 500).
    async fn get_builtin_tools(&self, agent_id: &str) -> BuiltinToolsResponse;

    /// `PUT /agents/{id}/builtin-tools` — persist the **complete**
    /// enabled-set for builtin tools. Performs the standard
    /// read-modify-write cycle via
    /// [`crate::agent_config::apply_builtin_tools_patch`], which:
    ///   - flips every listed name to `enabled = true`,
    ///   - flips every **other** currently-persisted tool to
    ///     `enabled = false`,
    ///   - silently ignores names not present in the current config
    ///     (defensive: future tools arrive via `merge_tools_config` at
    ///     startup, not via this incremental path),
    ///   - force-enables [`crate::tools::registry::PLATFORM_PROTECTED_TOOLS`]
    ///     regardless of the patch.
    ///
    /// No `UnknownTools` error variant — unlike MCP, unknown names are
    /// accepted-and-dropped (see ADR-029 §7).
    async fn put_builtin_tools(
        &self,
        agent_id: &str,
        body: PutBuiltinToolsBody,
    ) -> Result<BuiltinToolsResponse, AgentToolsError>;

    /// `GET /agents/{id}/tools` - merged Tools-panel view (builtin +
    /// MCP + search). Read-only aggregation of the three Tools-panel
    /// config files; the handler does not merge anything itself.
    async fn get_merged_tools(&self, agent_id: &str) -> MergedToolsResponse;
}