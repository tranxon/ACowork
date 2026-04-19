//! Agent Runtime configuration

use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Runtime configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Agent ID (reverse-domain identifier)
    pub agent_id: String,
    /// Path to .agent package (ZIP or directory)
    pub package_path: String,
    /// Working directory for the agent
    pub work_dir: String,
    /// Gateway endpoint (e.g., unix:///tmp/agent-gateway.sock)
    pub gateway_endpoint: String,
    /// Path to manifest.toml override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    /// Config directory for the agent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    /// Whether developer mode is enabled
    #[serde(default)]
    pub dev_mode: bool,
    /// Log level
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Maximum iterations per conversation
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Iteration timeout in milliseconds
    #[serde(default = "default_iteration_timeout_ms")]
    pub iteration_timeout_ms: u64,
    /// Maximum history tokens
    #[serde(default = "default_history_max_tokens")]
    pub history_max_tokens: u64,
    /// Tool result folding: keep last N iterations complete
    #[serde(default = "default_keep_full_results")]
    pub keep_full_results: usize,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_max_iterations() -> u32 {
    20
}

fn default_iteration_timeout_ms() -> u64 {
    30000
}

fn default_history_max_tokens() -> u64 {
    128000
}

fn default_keep_full_results() -> usize {
    4
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            package_path: String::new(),
            work_dir: String::new(),
            gateway_endpoint: String::new(),
            manifest_path: None,
            config_dir: None,
            dev_mode: false,
            log_level: default_log_level(),
            max_iterations: default_max_iterations(),
            iteration_timeout_ms: default_iteration_timeout_ms(),
            history_max_tokens: default_history_max_tokens(),
            keep_full_results: default_keep_full_results(),
        }
    }
}

impl RuntimeConfig {
    /// Build RuntimeConfig from CLI arguments
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            agent_id: cli.agent_id.clone(),
            package_path: cli.package_path.clone(),
            work_dir: cli.work_dir.clone(),
            gateway_endpoint: cli.gateway_endpoint.clone(),
            manifest_path: cli.manifest_path.clone(),
            config_dir: cli.config_dir.clone(),
            dev_mode: cli.dev_mode,
            log_level: cli.log_level.clone(),
            ..Default::default()
        }
    }
}
