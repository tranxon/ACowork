//! Per-agent runtime configuration types.
//!
//! NOTE: Per-agent config persistence has been moved to Runtime
//! ({work_dir}/config/agent_config.json). Gateway only defines the
//! request/response DTOs and forwards queries to Runtime via IPC.

use serde::{Deserialize, Serialize};

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
}

/// Default global values used as fallback when no override exists.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;
pub const DEFAULT_MAX_ITERATIONS: u32 = 200;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_SHELL_APPROVAL_THRESHOLD: ShellApprovalThreshold = ShellApprovalThreshold::Medium;
