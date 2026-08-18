//! Per-agent runtime configuration types.
//!
//! NOTE: Per-agent config persistence has been moved to Runtime
//! ({work_dir}/config/agent_config.json). Gateway only defines the
//! request/response DTOs and forwards queries to Runtime via gRPC.
//!
//! ADR-017: Avatar config is managed by the Runtime (agent_config.json).
//! The Gateway maintains a lightweight avatar cache file
//! ({data_dir}/avatar_cache.json) so that `list_agents` can return the
//! current avatar without a gRPC roundtrip, even when the agent is stopped.
//! The cache is updated on avatar change and synced from AgentHello.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use acowork_core::ShellApprovalThreshold;
use acowork_core::protocol::{AgentSearchConfig, McpServerConfigDef};

/// Effective (merged) config returned to API consumers.
#[derive(Debug, Clone, Serialize)]
pub struct AgentConfigResponse {
    pub agent_id: String,
    /// Effective max_output_tokens (per-agent override > global > hardcoded default)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Effective max_iterations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Effective temperature
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Source of the effective temperature value:
    /// "config" | "manifest" | "default"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_source: Option<String>,
    /// The manifest-level temperature — for frontend placeholder display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_temperature: Option<f32>,
    /// Effective context window cap (tokens). 0 = no limit.
    /// Resolved from: agent_config.json → manifest.llm.context_window → DEFAULT_CONTEXT_WINDOW.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Source of the effective context window value:
    /// "config" | "manifest" | "default"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_source: Option<String>,
    /// The manifest-level context window cap — for frontend placeholder display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_context_window: Option<u64>,
    /// The manifest-compiled system prompt (read-only, loaded by caller)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// User's system prompt override (None = use manifest default)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,
    /// Effective shell approval threshold
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,
    /// Current model name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Current provider name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Gateway global max_output_tokens limit
    pub global_max_output_tokens: u64,
    /// Active MCP server names for this agent (from workspace config)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_mcp_servers: Vec<String>,
    /// Per-agent search provider config (from workspace agent_search.json)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_config: Option<AgentSearchConfig>,
    /// ADR-024: max sessions limit per-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    /// ADR-023: Approval timeout in seconds. None = use system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout_secs: Option<u64>,
    /// ADR-029: Enabled builtin tool names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_tools: Vec<String>,
    /// ADR-029: Full builtin tools list with enabled flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_tools_all: Option<serde_json::Value>,
    /// ADR-052: Whether context_retrieve + context_abandon tools are registered.
    /// None = not set / use default (true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_compression_enabled: Option<bool>,
}

/// PUT request body for updating agent config.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateAgentConfigRequest {
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default, alias = "tools_limit")]
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default)]
    pub shell_approval_threshold: Option<ShellApprovalThreshold>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServerConfigDef>>,
    /// ADR-024: max sessions limit per-agent (0 = use default).
    #[serde(default)]
    pub max_sessions: Option<usize>,
    /// ADR-026: Per-agent context window cap (0 = no limit).
    #[serde(default)]
    pub context_window: Option<u64>,
    /// ADR-023: Approval timeout in seconds. None = use system default.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,
    /// ADR-029: Enabled builtin tool names to set.
    /// Some(vec![]) disables all builtin tools. None leaves unchanged.
    #[serde(default)]
    pub builtin_tools: Option<Vec<String>>,
    /// ADR-052: Whether context_retrieve + context_abandon tools are registered.
    /// JSON `null` / field absent -> "don't change" (partial-PUT semantics).
    /// JSON `true` or `false` -> explicit value, persisted verbatim.
    /// Hot-reloadable: the Runtime pushes the change to the SessionManager
    /// via `apply_runtime_config_override`, which updates the shared
    /// `AgentCore` template (future sessions inherit it), the
    /// `runtime_overrides` cache, and every active SessionTask's
    /// `ContextBuilder.tool_definitions` in one shot (ADR-052 §3.5).
    #[serde(default)]
    pub tool_compression_enabled: Option<bool>,
}

/// Default global values used as fallback when no override exists.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;
pub const DEFAULT_MAX_ITERATIONS: u32 = 200;
/// Default LLM temperature (final fallback in the chain
/// session → agent_config.json → manifest.toml [llm].temperature → here).
/// **Keep aligned** with `acowork_runtime::config::DEFAULT_TEMPERATURE`
/// so the Gateway HTTP API and Runtime resolve to the same value when both
/// manifest and override are absent.
pub const DEFAULT_TEMPERATURE: f32 = 0.3;
/// Default context window cap (tokens) — final fallback in the per-agent
/// chain: agent_config.json → manifest.llm.context_window → here.
/// **Keep aligned** with `acowork_runtime::config::DEFAULT_CONTEXT_WINDOW`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const DEFAULT_SHELL_APPROVAL_THRESHOLD: ShellApprovalThreshold = ShellApprovalThreshold::Medium;

// ── Avatar config DTOs (ADR-017) ────────────────────────────────────────

/// Effective avatar configuration returned by `GET /api/agents/:id/avatar-config`.
#[derive(Debug, Clone, Serialize)]
pub struct AvatarConfigResponse {
    pub agent_id: String,
    /// Effective custom avatar path (relative to install dir). Null when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Effective builtin avatar icon ID (e.g. "icon-05"). Null when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    /// Source of the effective value: "config" | "manifest" | "fallback".
    pub source: String,
}

/// PUT request body for `PUT /api/agents/:id/avatar-config`.
///
/// Semantics:
/// - Non-empty string → set to that value
/// - Empty string `""` → clear the field
/// - `null` / absent → do not modify
///
/// Setting `avatar` auto-clears `builtin_avatar`, and vice versa.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateAvatarConfigRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
}

/// A single avatar asset file entry in the install directory.
#[derive(Debug, Clone, Serialize)]
pub struct AvatarAssetEntry {
    pub relative_path: String,
}

/// Response for `GET /api/agents/:id/manifest/avatar-assets`.
#[derive(Debug, Clone, Serialize)]
pub struct AvatarAssetsResponse {
    pub agent_id: String,
    pub assets: Vec<AvatarAssetEntry>,
}

// ── ADR-017: Avatar cache file (Gateway-owned) ─────────────────────────

/// Avatar cache entry stored in `{data_dir}/avatar_cache.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AvatarCacheEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
}

/// Avatar cache file: `{agent_id}` → `AvatarCacheEntry`.
pub type AvatarCache = HashMap<String, AvatarCacheEntry>;

/// Filename for the avatar cache in the Gateway's data directory.
const AVATAR_CACHE_FILE: &str = "avatar_cache.json";

/// Load the avatar cache from `{data_dir}/avatar_cache.json`.
/// Returns an empty map if the file does not exist or cannot be parsed.
pub fn load_avatar_cache(data_dir: &Path) -> AvatarCache {
    let path = data_dir.join(AVATAR_CACHE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => AvatarCache::new(),
    }
}

/// Save the avatar cache to `{data_dir}/avatar_cache.json` (atomic write).
pub fn save_avatar_cache(data_dir: &Path, cache: &AvatarCache) {
    let path = data_dir.join(AVATAR_CACHE_FILE);
    match serde_json::to_string_pretty(cache) {
        Ok(json) => {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Err(e) => tracing::warn!("Failed to serialize avatar cache: {}", e),
    }
}

/// Update a single agent's avatar in the cache file (read-modify-write).
pub fn update_avatar_in_cache(
    data_dir: &Path,
    agent_id: &str,
    avatar: Option<String>,
    builtin_avatar: Option<String>,
) {
    let mut cache = load_avatar_cache(data_dir);
    let entry = cache.entry(agent_id.to_string()).or_default();
    entry.avatar = avatar;
    entry.builtin_avatar = builtin_avatar;
    save_avatar_cache(data_dir, &cache);
}
