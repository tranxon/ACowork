//! Per-agent runtime configuration persistence.
//!
//! Stores per-agent config (max_output_tokens, max_iterations,
//! temperature, system_prompt_override, active_tools, shell_approval_threshold,
//! mcp_servers, available_models) as JSON in `{work_dir}/config/agent_config.json`.
//!
//! This replaces the former Gateway-side `data/agent_configs/{agent_id}.json`.
//! The Runtime is now the authoritative owner of per-agent configuration.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use rollball_core::protocol::McpServerConfigDef;

/// Per-agent configuration persisted to workspace/config/agent_config.json.
///
/// On first start, defaults are generated from manifest.toml and AgentHelloResult.
/// User modifications via the Desktop App are persisted here by the Runtime
/// when Gateway pushes a RuntimeConfigUpdate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Max output tokens per request (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    /// Max LLM iterations per run (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,

    /// LLM temperature (None = use global default 0.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// System prompt override (None = use manifest-compiled prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,

    /// Active tool names (from manifest + user overrides).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_tools: Vec<String>,

    /// Shell command approval threshold ("low" | "medium" | "high" | "never").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,

    /// Active MCP server configurations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerConfigDef>,

    /// Available models list (cached from last Gateway push).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: None,
            max_iterations: None,
            temperature: None,
            system_prompt_override: None,
            active_tools: Vec::new(),
            shell_approval_threshold: None,
            mcp_servers: Vec::new(),
            available_models: Vec::new(),
        }
    }
}

/// Filename for per-agent config in the workspace config directory.
const AGENT_CONFIG_FILE: &str = "agent_config.json";

/// Build the path to the agent config file.
fn config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_CONFIG_FILE)
}

/// Load per-agent config from workspace/config/agent_config.json.
///
/// Returns `None` if the file does not exist (first start).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_config(work_dir: &Path) -> Result<Option<AgentConfig>, String> {
    let path = config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        has_tools = !cfg.active_tools.is_empty(),
        has_mcp = !cfg.mcp_servers.is_empty(),
        "Loaded agent config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent config to workspace/config/agent_config.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_config(work_dir: &Path, cfg: &AgentConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir {}: {}", config_dir.display(), e))?;

    let path = config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path)
        .map_err(|e| format!("Failed to rename {} -> {}: {}", tmp_path.display(), path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        "Saved agent config to workspace"
    );

    Ok(())
}
