//! Per-agent runtime configuration persistence.
//!
//! Stores per-agent config overrides (max_output_tokens, max_iterations,
//! temperature, system_prompt_override) as JSON files under
//! `{data_dir}/agent_configs/{agent_id}.json`.
//!
//! These overrides are merged with Gateway-level defaults when serving
//! the GET /api/agents/{id}/config endpoint and pushed to the Runtime
//! via RuntimeConfigUpdate on connect and on PUT.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use rollball_core::protocol::McpServerConfigDef;
use rollball_core::ShellApprovalThreshold;

use crate::error::GatewayError;

/// Per-agent config override (persisted to disk).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfigOverride {
    /// Max output tokens per request (None = use global default)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Max LLM iterations per run (None = use global default).
    /// Controls the total number of LLM turns in a single Agent loop.
    #[serde(skip_serializing_if = "Option::is_none", alias = "tools_limit")]
    pub max_iterations: Option<u32>,
    /// LLM temperature override (None = use global default 0.7)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// System prompt override (None = use manifest-compiled prompt)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,
    /// Active tool names (None = use manifest [[tools]] declarations).
    /// Some(vec![]) means no tools active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tools: Option<Vec<String>>,
    /// Shell command approval threshold (None = use default Medium).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<ShellApprovalThreshold>,
    /// MCP server configurations (None = keep current).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<McpServerConfigDef>>,
}

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
    /// Active tool names (from manifest + config overrides)
    pub active_tools: Vec<String>,
    /// Effective shell approval threshold
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,
    /// Effective MCP server configurations (JSON strings from ConfigSnapshot)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    /// Available models list (from Gateway global resources)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    /// Current model name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Current provider name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Gateway global max_output_tokens limit
    pub global_max_output_tokens: u64,
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
    pub active_tools: Option<Vec<String>>,
    #[serde(default)]
    pub shell_approval_threshold: Option<ShellApprovalThreshold>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServerConfigDef>>,
}

/// Default global values used as fallback when no override exists.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;
pub const DEFAULT_MAX_ITERATIONS: u32 = 50;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_SHELL_APPROVAL_THRESHOLD: ShellApprovalThreshold = ShellApprovalThreshold::Medium;

/// Build the path to the per-agent config file.
fn config_path(data_dir: &Path, agent_id: &str) -> PathBuf {
    data_dir.join("agent_configs").join(format!("{}.json", agent_id))
}

/// Load per-agent config overrides from disk.
/// Returns `Ok(None)` if no config file exists for this agent.
pub fn load_agent_config(data_dir: &Path, agent_id: &str) -> Result<Option<AgentConfigOverride>, GatewayError> {
    let path = config_path(data_dir, agent_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to read agent config for {}: {}",
            agent_id, e
        ))
    })?;
    let cfg: AgentConfigOverride = serde_json::from_str(&raw).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to parse agent config for {}: {}",
            agent_id, e
        ))
    })?;
    Ok(Some(cfg))
}

/// Save per-agent config overrides to disk.
pub fn save_agent_config(
    data_dir: &Path,
    agent_id: &str,
    cfg: &AgentConfigOverride,
) -> Result<(), GatewayError> {
    let dir = data_dir.join("agent_configs");
    std::fs::create_dir_all(&dir).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to create agent_configs dir: {}",
            e
        ))
    })?;
    let path = config_path(data_dir, agent_id);
    let json = serde_json::to_string_pretty(cfg).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to serialize agent config for {}: {}",
            agent_id, e
        ))
    })?;
    std::fs::write(&path, json).map_err(|e| {
        GatewayError::Lifecycle(format!(
            "Failed to write agent config for {}: {}",
            agent_id, e
        ))
    })?;
    tracing::info!(agent_id = %agent_id, "Agent config saved");
    Ok(())
}

/// Merge per-agent override with global defaults to produce effective config.
///
/// NOTE: Deprecated by Phase 5 refactor. Per-agent config is now owned by Runtime.
/// This function remains for backward compatibility but returns a stub.
pub fn get_effective_config(
    agent_id: &str,
    _per_agent: Option<&AgentConfigOverride>,
    global_max_output_tokens: u64,
    _system_prompt: Option<String>,
    _manifest_active_tools: Vec<String>,
) -> AgentConfigResponse {
    AgentConfigResponse {
        agent_id: agent_id.to_string(),
        max_output_tokens: Some(global_max_output_tokens),
        max_iterations: Some(DEFAULT_MAX_ITERATIONS),
        temperature: Some(DEFAULT_TEMPERATURE),
        system_prompt: None,
        system_prompt_override: None,
        active_tools: vec![],
        shell_approval_threshold: Some("medium".to_string()),
        mcp_servers: vec![],
        available_models: vec![],
        model: None,
        provider: None,
        global_max_output_tokens,
    }
}
