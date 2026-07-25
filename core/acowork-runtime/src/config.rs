//! Agent Runtime configuration

use serde::{Deserialize, Serialize};

use acowork_core::timeout_config::constants;
use acowork_core::Timeouts;

use crate::cli::Cli;

/// Default HTTP timeout for built-in tools (30 seconds).
///
/// Centralized in `acowork_core::timeout_config::constants::TOOL_HTTP`.
/// Re-exported here for backward compatibility with search backends and
/// `web_fetch` that use `DEFAULT_TOOL_HTTP_TIMEOUT` as a fallback default.
pub const DEFAULT_TOOL_HTTP_TIMEOUT: std::time::Duration = constants::TOOL_HTTP;

/// Millisecond equivalent of [`DEFAULT_TOOL_HTTP_TIMEOUT`].
pub const DEFAULT_TOOL_HTTP_TIMEOUT_MS: u64 = DEFAULT_TOOL_HTTP_TIMEOUT.as_millis() as u64;

/// Default LLM temperature applied as the final fallback in the per-agent
/// resolution chain (Layer 3):
/// `agent_config.json.temperature` (Layer 1) →
/// `manifest.llm.temperature` (Layer 2) →
/// **this constant** (Layer 3).
///
/// This is the hardcoded final layer when neither the runtime user override
/// nor the package-level manifest default is set. Keep aligned with the
/// `DEFAULT_TEMPERATURE` constant in `acowork-gateway/src/http/agent_config.rs`
/// (the Gateway uses the same value when constructing `AgentConfigResponse`).
pub const DEFAULT_TEMPERATURE: f32 = 0.3;

/// Default context window cap for per-agent resolution chain.
///
/// 200K tokens covers the majority of current flagship models
/// (GPT-4o 128K, Claude Sonnet 200K, DeepSeek-V3 128K).
/// This is the hardcoded final fallback when neither the user's
/// agent_config.json override nor the package-level manifest default is set.
/// **Keep aligned** with `acowork_gateway::http::agent_config::DEFAULT_CONTEXT_WINDOW`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Runtime configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Agent ID (reverse-domain identifier)
    pub agent_id: String,
    /// Path to .agent package (ZIP or directory)
    pub package_path: String,
    /// Working directory for the agent
    pub work_dir: String,
    /// ADR-033: MQTT broker port for Runtime MQTT client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mqtt_port: Option<u16>,
    /// ADR-033: Runtime localhost HTTP server port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
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
    /// Maximum number of log files to keep (0 = unlimited, default 20)
    #[serde(default = "default_log_file_count")]
    pub log_file_count: u64,
    /// Maximum iterations per conversation
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Centralized timeout configuration flattened into the historical TOML keys.
    #[serde(flatten)]
    pub timeouts: Timeouts,
    /// Maximum history tokens
    #[serde(default = "default_history_max_tokens")]
    pub history_max_tokens: u64,
    /// Shell approval threshold: Low / Medium / High / Never
    /// Controls which shell commands require user confirmation.
    /// Default: "medium" — Medium and High risk commands need approval.
    #[serde(default = "default_shell_approval_threshold")]
    pub shell_approval_threshold: String,

    /// Maximum number of session files (JSONL) to keep on disk.
    /// When this limit is exceeded at session creation, the oldest
    /// sessions (by `last_active_at`) are permanently deleted.
    /// Set to 0 to disable the limit.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Minimum character length of formatted conversation text before we
    /// bother running LLM summarization on session close. Shorter sessions
    /// use the raw text directly as their episode summary.
    #[serde(default = "default_min_distill_chars")]
    pub min_distill_chars: usize,
    /// Max output tokens for LLM summarization calls (compaction + distillation).
    #[serde(default = "default_distill_max_tokens")]
    pub distill_max_tokens: u32,
    /// Data flow tuning parameters (ADR-020: channel capacities, flush intervals).
    #[serde(default)]
    pub data_flow: DataFlowConfig,
}

/// Data flow tuning configuration (ADR-020).
///
/// Controls channel capacities and flush intervals for the Runtime's
/// internal data pipelines. These values affect throughput and latency
/// under load — especially during LLM streaming (thinking mode).
///
/// ADR-021: Data channel removed; only control channel remains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataFlowConfig {
    /// Capacity of the chunk mpsc channel (control events).
    /// Default: 64.
    #[serde(default = "default_chunk_capacity")]
    pub chunk_capacity: usize,
    /// Capacity of the gRPC outbound control mpsc channel (Done, Error,
    /// Stopped, SessionStateChanged, etc.).
    /// Default: 256.
    #[serde(default = "default_outbound_ctrl_capacity")]
    pub outbound_ctrl_capacity: usize,
    /// Reasoning token batch flush interval in milliseconds.
    /// Tokens are accumulated and flushed at this interval to reduce
    /// channel write frequency during thinking mode. Default: 200.
    #[serde(default = "default_reasoning_flush_interval_ms")]
    pub reasoning_flush_interval_ms: u64,
    /// Minimum interval in milliseconds between NewDataAvailable
    /// notifications.  Also used by the frontend PollingManager as the
    /// initial polling interval and typewriter animation duration.
    /// Default: 500 (max ~2 notifications/sec, matching polling rate).
    #[serde(default = "default_notify_interval_ms")]
    pub notify_interval_ms: u64,
}

fn default_chunk_capacity() -> usize {
    // Matches `outbound_ctrl_capacity` for headroom during cold-start bursts
    // (relay drains serial publish tasks one at a time). The chunk channel
    // only fills under abnormal backpressure — see ADR-035 ordering notes.
    256
}
fn default_outbound_ctrl_capacity() -> usize {
    256
}
fn default_reasoning_flush_interval_ms() -> u64 {
    200
}
fn default_notify_interval_ms() -> u64 {
    500
}

impl Default for DataFlowConfig {
    fn default() -> Self {
        Self {
            chunk_capacity: default_chunk_capacity(),
            outbound_ctrl_capacity: default_outbound_ctrl_capacity(),
            reasoning_flush_interval_ms: default_reasoning_flush_interval_ms(),
            notify_interval_ms: default_notify_interval_ms(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_file_size_mb() -> u64 {
    10
}

fn default_log_file_count() -> u64 {
    20
}

fn default_debug_port() -> u16 {
    19878
}

fn default_max_iterations() -> u32 {
    200
}

fn default_history_max_tokens() -> u64 {
    128000
}

fn default_shell_approval_threshold() -> String {
    "medium".to_string()
}

fn default_max_sessions() -> usize {
    2000
}

fn default_min_distill_chars() -> usize {
    8000
}

fn default_distill_max_tokens() -> u32 {
    2000
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            package_path: String::new(),
            work_dir: String::new(),
            mqtt_port: None,
            http_port: None,
            manifest_path: None,
            config_dir: None,
            dev_mode: false,
            debug_port: default_debug_port(),
            log_level: default_log_level(),
            log_file_size_mb: default_log_file_size_mb(),
            log_file_count: default_log_file_count(),
            max_iterations: default_max_iterations(),
            timeouts: Timeouts::default(),
            history_max_tokens: default_history_max_tokens(),
            shell_approval_threshold: default_shell_approval_threshold(),
            max_sessions: default_max_sessions(),
            min_distill_chars: default_min_distill_chars(),
            distill_max_tokens: default_distill_max_tokens(),
            data_flow: DataFlowConfig::default(),
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
            manifest_path: cli.manifest_path.clone(),
            config_dir: cli.config_dir.clone(),
            dev_mode: cli.dev_mode,
            debug_port: cli.debug_port,
            log_level: cli.log_level.clone(),
            log_file_size_mb: cli.log_file_size_mb,
            log_file_count: cli.log_file_count,
            mqtt_port: cli.mqtt_port,
            http_port: cli.http_port,
            ..Default::default()
        }
    }

    /// Validate startup-sensitive configuration values.
    pub fn validate(&self) -> Result<(), String> {
        acowork_core::timeout_config::validate(&self.timeouts)
    }
}
