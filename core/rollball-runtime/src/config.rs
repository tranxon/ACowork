//! Agent Runtime configuration

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::cli::Cli;

/// Default HTTP timeout for built-in tools (30 seconds).
pub const DEFAULT_TOOL_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Default HTTP timeout for built-in tools as Duration.
pub const DEFAULT_TOOL_HTTP_TIMEOUT: Duration = Duration::from_millis(DEFAULT_TOOL_HTTP_TIMEOUT_MS);

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
    /// Deprecated: use `gateway_socket` instead.
    #[deprecated(note = "Use gateway_socket instead. Will be removed in a future version.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_endpoint: Option<String>,
    /// Gateway Unix socket path for IPC connection
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_socket: Option<String>,
    /// Path to manifest.toml override
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    /// Config directory for the agent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    /// Whether developer mode is enabled
    #[serde(default)]
    pub dev_mode: bool,
    /// Debug WebSocket server port (used with dev_mode)
    #[serde(default = "default_debug_port")]
    pub debug_port: u16,
    /// Log level
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Log file size in MB before auto-split (0 = no split)
    #[serde(default = "default_log_file_size_mb")]
    pub log_file_size_mb: u64,
    /// Maximum iterations per conversation
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Iteration timeout in milliseconds (overall timeout for the entire iteration)
    #[serde(default = "default_iteration_timeout_ms")]
    pub iteration_timeout_ms: u64,
    /// Single tool execution timeout in milliseconds
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
    /// Maximum history tokens
    #[serde(default = "default_history_max_tokens")]
    pub history_max_tokens: u64,
    /// Shell approval threshold: Low / Medium / High / Never
    /// Controls which shell commands require user confirmation.
    /// Default: "medium" — Medium and High risk commands need approval.
    #[serde(default = "default_shell_approval_threshold")]
    pub shell_approval_threshold: String,

    // ── Timeout configuration ──

    /// LLM provider HTTP request timeout in milliseconds
    #[serde(default = "default_provider_request_timeout_ms")]
    pub provider_request_timeout_ms: u64,
    /// LLM provider TCP connect timeout in milliseconds
    #[serde(default = "default_provider_connect_timeout_ms")]
    pub provider_connect_timeout_ms: u64,
    /// LLM provider stream read per-chunk timeout in milliseconds
    #[serde(default = "default_provider_stream_read_timeout_ms")]
    pub provider_stream_read_timeout_ms: u64,
    /// Default HTTP timeout for built-in tools in milliseconds
    #[serde(default = "default_tool_http_timeout_ms")]
    pub tool_http_timeout_ms: u64,
    /// Session idle timeout in seconds before eviction
    #[serde(default = "default_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,
    /// Minimum character length of formatted conversation text before we
    /// bother running LLM summarization on session close. Shorter sessions
    /// use the raw text directly as their episode summary.
    #[serde(default = "default_min_distill_chars")]
    pub min_distill_chars: usize,
    /// Max output tokens for LLM summarization calls (compaction + distillation).
    #[serde(default = "default_distill_max_tokens")]
    pub distill_max_tokens: u32,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_file_size_mb() -> u64 {
    10
}

fn default_debug_port() -> u16 {
    19878
}

fn default_max_iterations() -> u32 {
    50
}

fn default_iteration_timeout_ms() -> u64 {
    900000
}

fn default_tool_timeout_ms() -> u64 {
    600000
}

fn default_history_max_tokens() -> u64 {
    128000
}

fn default_shell_approval_threshold() -> String {
    "medium".to_string()
}

fn default_provider_request_timeout_ms() -> u64 {
    600000 // 10 min: LLM streaming can be long (thinking + generation)
}

fn default_provider_connect_timeout_ms() -> u64 {
    10000 // 10 sec
}

fn default_provider_stream_read_timeout_ms() -> u64 {
    45000 // 45 sec per-chunk interval
}

fn default_tool_http_timeout_ms() -> u64 {
    DEFAULT_TOOL_HTTP_TIMEOUT_MS
}

fn default_session_idle_timeout_secs() -> u64 {
    300 // 5 min
}

fn default_min_distill_chars() -> usize {
    8000
}

fn default_distill_max_tokens() -> u32 {
    2048
}

impl Default for RuntimeConfig {
    #[allow(deprecated)]
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            package_path: String::new(),
            work_dir: String::new(),
            gateway_endpoint: None,
            gateway_socket: None,
            manifest_path: None,
            config_dir: None,
            dev_mode: false,
            debug_port: default_debug_port(),
            log_level: default_log_level(),
            log_file_size_mb: default_log_file_size_mb(),
            max_iterations: default_max_iterations(),
            iteration_timeout_ms: default_iteration_timeout_ms(),
            tool_timeout_ms: default_tool_timeout_ms(),
            history_max_tokens: default_history_max_tokens(),
            shell_approval_threshold: default_shell_approval_threshold(),
            provider_request_timeout_ms: default_provider_request_timeout_ms(),
            provider_connect_timeout_ms: default_provider_connect_timeout_ms(),
            provider_stream_read_timeout_ms: default_provider_stream_read_timeout_ms(),
            tool_http_timeout_ms: default_tool_http_timeout_ms(),
            session_idle_timeout_secs: default_session_idle_timeout_secs(),
            min_distill_chars: default_min_distill_chars(),
            distill_max_tokens: default_distill_max_tokens(),
        }
    }
}

impl RuntimeConfig {
    /// Build RuntimeConfig from CLI arguments
    pub fn from_cli(cli: &Cli) -> Self {
        #[allow(deprecated)]
        let gateway_endpoint = cli.gateway_endpoint.clone();
        #[allow(deprecated)]
        Self {
            agent_id: cli.agent_id.clone(),
            package_path: cli.package_path.clone(),
            work_dir: cli.work_dir.clone(),
            gateway_endpoint,
            gateway_socket: cli.gateway_socket.clone(),
            manifest_path: cli.manifest_path.clone(),
            config_dir: cli.config_dir.clone(),
            dev_mode: cli.dev_mode,
            debug_port: cli.debug_port,
            log_level: cli.log_level.clone(),
            log_file_size_mb: cli.log_file_size_mb,
            ..Default::default()
        }
    }

    /// Get gateway address with priority: gateway_socket > gateway_endpoint.
    /// Returns None if neither is set (standalone mode).
    #[allow(deprecated)]
    pub fn get_gateway_address(&self) -> Option<&str> {
        if let Some(ref socket) = self.gateway_socket {
            return Some(socket.as_str());
        }
        if let Some(ref endpoint) = self.gateway_endpoint {
            return Some(endpoint.as_str());
        }
        None
    }
}
