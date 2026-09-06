//! Gateway configuration
//!
//! Configuration can come from:
//! 1. CLI arguments (highest priority)
//! 2. Environment variables
//! 3. Config file (gateway.toml)
//! 4. Defaults (lowest priority)

use std::path::PathBuf;

use crate::cli::Cli;
use crate::error::GatewayError;
use acowork_core::{Timeouts, defaults};
use serde::{Deserialize, Serialize};

/// Compute the single application root directory for the gateway.
///
/// Layout (all platforms):
/// ```text
/// <root>/
/// ├── config/      # vault, packages, socket, gateway.toml, gateway logs
/// └── data/        # resource caches, models, gateway.pid, embed logs
/// ```
///
/// On Linux/macOS the default is `$HOME/.acowork/acowork-gateway/`.
/// On Windows the default is `%USERPROFILE%\.acowork\acowork-gateway\`.
///
/// Override with the `ACOWORK_HOME` environment variable or the
/// `--home` CLI flag (useful for tests and power users).
///
/// Replaces the previous `directories::ProjectDirs` setup which split
/// config and data across `~/.config/` and `~/.local/share/` on Linux,
/// `%APPDATA%` subdirs on Windows, and a single dir on macOS.
pub(crate) fn project_root() -> PathBuf {
    if let Ok(p) = std::env::var("ACOWORK_HOME")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }

    #[cfg(windows)]
    let home_var = std::env::var("USERPROFILE").ok();
    #[cfg(not(windows))]
    let home_var = std::env::var("HOME").ok();

    match home_var {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".acowork").join("acowork-gateway"),
        _ => PathBuf::from(".").join(".acowork-gateway"),
    }
}

/// Gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Path to the TOML config file this was loaded from (if any).
    /// Used by `save()` to persist runtime config changes back to disk.
    /// Skipped in serialization — not stored in the TOML file itself.
    #[serde(skip)]
    pub config_source_path: Option<String>,
    /// Vault directory for encrypted key storage
    pub vault_dir: String,
    /// Packages directory for installed .agent packages
    pub packages_dir: String,
    /// Data directory for agent workspaces and Grafeo
    pub data_dir: String,
    /// Log level (trace/debug/info/warn/error)
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Log file maximum size in MB before auto-split (0 = no split, default 10)
    #[serde(default = "default_log_file_size_mb")]
    pub log_file_size_mb: u64,
    /// Maximum number of log files to keep (0 = unlimited, default 20)
    #[serde(default = "default_log_file_count")]
    pub log_file_count: u64,
    /// Centralized timeout configuration flattened into the historical TOML keys.
    #[serde(flatten)]
    pub timeouts: Timeouts,
    /// Default max iterations per agent run
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Development mode: allows unsigned packages, relaxed security
    #[serde(default)]
    pub dev_mode: bool,
    /// HTTP API configuration
    #[serde(default)]
    pub http: HttpConfig,
    /// Default LLM provider for agents
    /// When set, Gateway delivers this provider's config to agents via gRPC.
    /// If not set, falls back to the first key stored in Vault.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default LLM model for agents
    /// When set, Gateway delivers this model to agents via gRPC.
    /// If not set, falls back to the Vault entry's default_model.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Global max output tokens limit for all agents.
    /// When a model's max_output_tokens exceeds this value, the value is capped.
    /// Default: 32768 (32K). Set to 0 to disable the limit.
    #[serde(default = "default_max_output_tokens_limit")]
    pub max_output_tokens_limit: u64,
    /// HuggingFace mirror URLs for model downloads (tried in order before
    /// the official `huggingface.co`). Empty list = official site only.
    /// Example in TOML: `hf_mirrors = ["https://hf-mirror.com"]`
    #[serde(default)]
    pub hf_mirrors: Vec<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Data flow tuning parameters (ADR-020: channel capacities, thread counts).
    #[serde(default)]
    pub data_flow: DataFlowConfig,
    /// MQTT broker configuration (ADR-033).
    #[serde(default)]
    pub mqtt: MqttConfig,
    /// Advertise host: the address other machines should use to reach
    /// services on this Gateway host (embed / LSP / broker endpoints
    /// distributed to Runtime and Desktop via MQTT).
    ///
    /// Distinct from `mqtt.host` (bind address) — the standard split for
    /// distributed systems (Docker / K8s advertise-addr). Bind may be
    /// `0.0.0.0` while advertise must be a routable address.
    ///
    /// If unset at startup, Gateway auto-detects the first non-loopback
    /// IP and logs a WARN (ADR-055 D3 §6.3). Defaults to `127.0.0.1`
    /// when detection fails (single-machine compatibility).
    #[serde(default)]
    pub advertise_host: Option<String>,
    /// Optional override for the local node's reverse-proxy port
    /// (`acowork-node --proxy-port`, default 19900). Lets a second
    /// Gateway instance on the same machine (tests, previews) run its
    /// own node without clashing with the primary instance's proxy.
    #[serde(default)]
    pub node_proxy_port: Option<u16>,
    /// Optional override for the local node's LSP relay sidecar port
    /// (default 19878). See `node_proxy_port`.
    #[serde(default)]
    pub node_lsp_relay_port: Option<u16>,
    /// PM 项目管理服务配置（ADR-064）。
    ///
    /// PM 作为**独立进程**运行（`acowork-pm` 二进制），由 Gateway supervisor
    /// 管理生命周期，Gateway 反向代理 `/api/pm/*` → `127.0.0.1:{pm.port}/*`。
    /// 本段只保留 Gateway 需要的字段（是否启用 / 端口 / MCP 注入）；PM 自身的
    /// 调优参数（max_task_depth 等）由 PM 进程独立解析（env / TOML / default）。
    #[serde(default)]
    pub pm: PmConfig,

    /// Online document library service (acowork-doc, standalone process).
    ///
    /// Mirrors the PM pattern (ADR-064): the doc service runs as an
    /// independent binary supervised by the Gateway, which reverse-proxies
    /// `/api/doc/*` → `127.0.0.1:{doc.port}/*`. This section only keeps the
    /// fields the Gateway needs to manage the process; the doc process
    /// itself parses its tuning parameters independently.
    #[serde(default)]
    pub doc: DocConfig,
}

/// PM 服务配置（Gateway 侧，ADR-064）。
///
/// 仅包含 Gateway 管理 PM 独立进程所需的字段。PM 进程自身的完整配置
/// （数据目录、任务深度、附件限额等）由 `acowork-pm` 独立解析，不再嵌入
/// Gateway 配置树。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmConfig {
    /// 是否启用 PM 服务（spawn 独立进程）。
    ///
    /// 默认 `true`。`false` 时 Gateway 不 spawn PM，`/api/pm/*` 返回 503。
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// PM 独立进程监听端口。
    ///
    /// 默认 `18082`。端口冲突时 PM 自动递增（最多 +20），实际端口经
    /// `--port-file` 上报，Gateway supervisor 读取后写入 `pm_process`。
    #[serde(default = "default_pm_port")]
    pub port: u16,

    /// 是否自动把 pm MCP HTTP 端点注入每个 Agent 的 MCP catalog（设计 §6.1 / T3-4）。
    ///
    /// 默认 `true`：Gateway 在 `acowork/global/mcps` 资源下发中附带一个
    /// `name = "pm"`、transport = http 的 MCP server，Agent 启动后自动获得
    /// `pm_*` 工具。关闭后 Agent 需在 Tools 面板手动添加（通常无需关闭）。
    #[serde(default = "default_true")]
    pub auto_inject_mcp: bool,

    /// pm MCP HTTP 端点的公开路径（经 Gateway 反代）。默认 `/api/pm/mcp`（设计 §21）。
    #[serde(default = "default_pm_mcp_http_path")]
    pub mcp_http_path: String,
}

fn default_pm_port() -> u16 {
    18082
}

fn default_pm_mcp_http_path() -> String {
    "/api/pm/mcp".to_string()
}

impl Default for PmConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            port: default_pm_port(),
            auto_inject_mcp: default_true(),
            mcp_http_path: default_pm_mcp_http_path(),
        }
    }
}

/// doc service config (Gateway side).
///
/// Only fields the Gateway needs to manage the acowork-doc standalone
/// process (spawn / port / MCP injection). The doc process resolves its own
/// full config independently (`$HOME/.acowork/acowork-doc/` defaults);
/// `data_dir` and `request_ttl_hours` are optional overrides forwarded to
/// the subprocess via CLI flags when present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocConfig {
    /// Whether to spawn the acowork-doc subprocess.
    ///
    /// Default `true`. `false` → Gateway does not spawn doc and `/api/doc/*`
    /// returns 503.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Desired doc listen port.
    ///
    /// Default `18081`. The doc process auto-increments on conflict (max
    /// +20); the actual port is reported via `--port-file` and read into
    /// `doc_process` by the supervisor.
    #[serde(default = "default_doc_port")]
    pub port: u16,

    /// Optional data-directory override forwarded to the subprocess
    /// (`--data-dir`). `None` → doc resolves `$HOME/.acowork/acowork-doc/`.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,

    /// Whether to auto-inject the doc MCP HTTP endpoint into every Agent's
    /// MCP catalog (`auto_inject_mcp`, default `true`).
    #[serde(default = "default_true")]
    pub auto_inject_mcp: bool,

    /// Public path of the doc MCP endpoint (via Gateway reverse proxy).
    /// Default `/api/doc/mcp`.
    #[serde(default = "default_doc_mcp_http_path")]
    pub mcp_http_path: String,

    /// Optional update-request TTL override (hours) forwarded to the
    /// subprocess (`--request-ttl-hours`). `None` → doc default (72h).
    #[serde(default)]
    pub request_ttl_hours: Option<u32>,
}

fn default_doc_port() -> u16 {
    18081
}

fn default_doc_mcp_http_path() -> String {
    "/api/doc/mcp".to_string()
}

impl Default for DocConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            port: default_doc_port(),
            data_dir: None,
            auto_inject_mcp: default_true(),
            mcp_http_path: default_doc_mcp_http_path(),
            request_ttl_hours: None,
        }
    }
}

/// MQTT broker configuration (ADR-033).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MqttConfig {
    /// Whether the embedded MQTT broker is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Broker listen host (bind address).
    ///
    /// Defaults to `127.0.0.1` (localhost-only) — single-machine topology.
    /// For distributed deployments (ADR-055), set to `0.0.0.0` (all
    /// interfaces) or a specific NIC IP so remote Runtime / Desktop can
    /// connect. Pair with `advertise_host` so other machines know which
    /// routable address to use — bind may be `0.0.0.0` but advertise
    /// must be a specific routable IP (L3-7).
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    /// Broker listen port.
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    /// ADR-055 Phase 5a: enable the CONNECT-layer authentication model
    /// (enrollment tokens + per-node tokens + internal publisher / HTTP
    /// credentials). Defaults to **false** — the deployment must turn
    /// it on explicitly (after issuing enrollment tokens via
    /// `nodes token create`).
    #[serde(default)]
    pub auth_enabled: bool,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: default_mqtt_host(),
            port: default_mqtt_port(),
            auth_enabled: false,
        }
    }
}

fn default_true() -> bool { true }
fn default_mqtt_host() -> String { "127.0.0.1".to_string() }
/// Exposed as `pub(crate)` so the HTTP `/api/status` handler can report
/// the broker port to the Desktop without duplicating the constant
/// (ADR-055 D3 §6.3, L3-6).
pub(crate) fn default_mqtt_port() -> u16 { 19875 }

/// Data flow tuning configuration (ADR-020).
///
/// Controls channel capacities and concurrency limits for the Gateway's
/// internal data pipelines. These values affect throughput and latency
/// under load — especially during LLM streaming (thinking mode).
///
/// P2 (ADR-020): Bridge channel split into data (L1: LLM chunks) and
/// ctrl (L2/L3/L4: tools, control, metadata) for physical isolation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataFlowConfig {
    /// Number of tokio async worker threads for the Gateway runtime.
    /// Default: 8 (increased from 4 per ADR-020 P0-2).
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    /// Capacity of the Bridge control broadcast channel (all events:
    /// ToolCall, Done, Error, Stopped, SessionStateChanged, NewDataAvailable, etc.).
    /// ADR-021 Phase 2: data channel removed — single channel for all events.
    /// Default: 256 (control events are low-frequency; data events now via HTTP poll).
    #[serde(default = "default_bridge_ctrl_capacity")]
    pub bridge_ctrl_capacity: usize,
    /// Capacity of the capability broadcast channel.
    /// Default: 64.
    #[serde(default = "default_capability_broadcast_capacity")]
    pub capability_broadcast_capacity: usize,
}

fn default_worker_threads() -> usize {
    8
}
fn default_bridge_ctrl_capacity() -> usize {
    256
}
fn default_capability_broadcast_capacity() -> usize {
    64
}

impl Default for DataFlowConfig {
    fn default() -> Self {
        Self {
            worker_threads: default_worker_threads(),
            bridge_ctrl_capacity: default_bridge_ctrl_capacity(),
            capability_broadcast_capacity: default_capability_broadcast_capacity(),
        }
    }
}

/// HTTP API configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Enable the HTTP API server
    #[serde(default = "default_http_enabled")]
    pub enabled: bool,
    /// Host to bind (typically 127.0.0.1 for localhost-only)
    #[serde(default = "default_http_host")]
    pub host: String,
    /// Port to listen on (0 = auto-assign, 19876 = default)
    #[serde(default = "default_http_port")]
    pub port: u16,
    /// Maximum port when auto-incrementing on conflict
    #[serde(default = "default_http_port_max")]
    pub port_max: u16,
    /// Enable auth token (generates random token on start)
    #[serde(default)]
    pub auth_enabled: bool,
}

fn default_http_enabled() -> bool {
    true
}
fn default_http_host() -> String {
    defaults::GATEWAY_HTTP_HOST.to_string()
}
/// Default HTTP listen port; `pub(crate)` so the liveness handler
/// (`/health`) can fall back to it when no config snapshot exists.
pub(crate) fn default_http_port() -> u16 {
    defaults::GATEWAY_HTTP_PORT
}
fn default_http_port_max() -> u16 {
    defaults::GATEWAY_HTTP_PORT_MAX
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: default_http_enabled(),
            host: default_http_host(),
            port: default_http_port(),
            port_max: default_http_port_max(),
            auth_enabled: false,
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
fn default_max_iterations() -> u32 {
    20
}
fn default_max_output_tokens_limit() -> u64 {
    32_768
}

impl GatewayConfig {
    /// Get the config directory: `<root>/config/`
    pub(crate) fn project_config_dir() -> std::path::PathBuf {
        project_root().join("config")
    }

    /// Get the data directory: `<root>/data/`
    pub(crate) fn project_data_dir() -> std::path::PathBuf {
        project_root().join("data")
    }

    /// One-time migration from the previous split layout.
    ///
    /// On first startup with the new code, if the old XDG paths exist
    /// and the new root does not, move the old contents into the new
    /// layout (Linux/macOS only — Windows legacy paths are different
    /// enough that users should move them manually):
    ///
    ///   - `$XDG_CONFIG_HOME/acowork-gateway/` (default `~/.config/`)
    ///     → `<root>/config/`
    ///   - `$XDG_DATA_HOME/acowork-gateway/`   (default `~/.local/share/`)
    ///     → `<root>/data/`
    ///
    /// Idempotent: if the new root already exists, this is a no-op so
    /// we never overwrite an established installation.
    ///
    /// MUST be called before `init_tracing` (which creates the new log
    /// dir and would make `new_root.exists()` true). Uses `eprintln!`
    /// for status messages because the tracing subscriber isn't set up
    /// yet at this point.
    pub(crate) fn migrate_legacy_layout() {
        let new_root = project_root();
        let _ = new_root.exists();

        #[cfg(not(windows))]
        {
            // The new root itself must exist before rename can target
            // <root>/config or <root>/data as destinations.
            if let Err(e) = std::fs::create_dir_all(&new_root) {
                eprintln!(
                    "[acowork-gateway] WARN: failed to create {}: {}. Skipping legacy migration.",
                    new_root.display(),
                    e
                );
                return;
            }

            if let Some(old) = legacy_config_dir()
                && old.exists()
            {
                let dest = new_root.join("config");
                match std::fs::rename(&old, &dest) {
                    Ok(()) => eprintln!(
                        "[acowork-gateway] Migrated legacy config dir: {} -> {}",
                        old.display(),
                        dest.display()
                    ),
                    Err(e) => eprintln!(
                        "[acowork-gateway] WARN: failed to migrate legacy config dir ({} -> {}): {}. Please move manually.",
                        old.display(),
                        dest.display(),
                        e
                    ),
                }
            }

            if let Some(old) = legacy_data_dir()
                && old.exists()
            {
                let dest = new_root.join("data");
                match std::fs::rename(&old, &dest) {
                    Ok(()) => eprintln!(
                        "[acowork-gateway] Migrated legacy data dir: {} -> {}",
                        old.display(),
                        dest.display()
                    ),
                    Err(e) => eprintln!(
                        "[acowork-gateway] WARN: failed to migrate legacy data dir ({} -> {}): {}. Please move manually.",
                        old.display(),
                        dest.display(),
                        e
                    ),
                }
            }
        }
    }

    /// Create config from CLI arguments
    pub fn from_cli(cli: &Cli) -> Result<Self, GatewayError> {
        // Try loading from config file first
        let file_config = if let Some(path) = &cli.config_path {
            Self::load_from_file(path)?
        } else {
            // Try default config location
            let default_path = Self::default_config_path()?;
            if default_path.exists() {
                Self::load_from_file(default_path.to_str().unwrap_or(""))?
            } else {
                None
            }
        };

        // Defaults
        let base_dir = Self::project_config_dir();
        let default_vault = base_dir.join("vault").to_string_lossy().to_string();
        let default_packages = base_dir.join("packages").to_string_lossy().to_string();

        let data_dir = Self::project_data_dir();
        let default_data = data_dir.to_string_lossy().to_string();

        // Determine config source path (for runtime persistence)
        let config_path = if let Some(path) = &cli.config_path {
            Some(path.clone())
        } else {
            Self::default_config_path()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        };

        // Merge: CLI > env > file > defaults
        let config = Self {
            config_source_path: config_path,
            vault_dir: cli
                .vault_dir
                .clone()
                .or(file_config.as_ref().map(|c| c.vault_dir.clone()))
                .unwrap_or(default_vault),
            packages_dir: cli
                .packages_dir
                .clone()
                .or(file_config.as_ref().map(|c| c.packages_dir.clone()))
                .unwrap_or(default_packages),
            data_dir: file_config
                .as_ref()
                .map(|c| c.data_dir.clone())
                .unwrap_or(default_data),
            log_level: if cli.log_level != "info" {
                cli.log_level.clone()
            } else {
                file_config
                    .as_ref()
                    .map(|c| c.log_level.clone())
                    .unwrap_or_else(default_log_level)
            },
            log_file_size_mb: file_config
                .as_ref()
                .map(|c| c.log_file_size_mb)
                .unwrap_or_else(default_log_file_size_mb),
            log_file_count: file_config
                .as_ref()
                .map(|c| c.log_file_count)
                .unwrap_or_else(default_log_file_count),
            timeouts: file_config
                .as_ref()
                .map(|c| c.timeouts.clone())
                .unwrap_or_default(),
            max_iterations: file_config
                .as_ref()
                .map(|c| c.max_iterations)
                .unwrap_or_else(default_max_iterations),
            dev_mode: file_config.as_ref().map(|c| c.dev_mode).unwrap_or(true),
            http: {
                let mut http = file_config
                    .as_ref()
                    .map(|c| c.http.clone())
                    .unwrap_or_default();
                // Allow ACOWORK_GATEWAY_HTTP_PORT env var to override the
                // configured port (used by E2E tests and manual testing).
                if let Ok(port_str) = std::env::var("ACOWORK_GATEWAY_HTTP_PORT")
                    && let Ok(port) = port_str.parse::<u16>()
                {
                    http.port = port;
                    // Ensure port_max is at least port+10 to allow
                    // auto-increment on conflict.
                    if http.port_max < port + 10 {
                        http.port_max = port + 10;
                    }
                }
                http
            },
            default_provider: file_config
                .as_ref()
                .and_then(|c| c.default_provider.clone()),
            default_model: file_config.as_ref().and_then(|c| c.default_model.clone()),
            max_output_tokens_limit: file_config
                .as_ref()
                .map(|c| c.max_output_tokens_limit)
                .unwrap_or_else(default_max_output_tokens_limit),
            hf_mirrors: file_config
                .as_ref()
                .map(|c| c.hf_mirrors.clone())
                .unwrap_or_default(),
            embedding_model: file_config.as_ref().and_then(|c| c.embedding_model.clone()),
            data_flow: file_config
                .as_ref()
                .map(|c| c.data_flow.clone())
                .unwrap_or_default(),
            mqtt: {
                let mut mqtt = file_config
                    .as_ref()
                    .map(|c| c.mqtt.clone())
                    .unwrap_or_default();
                // Allow ACOWORK_GATEWAY_MQTT_PORT env var to override the
                // configured broker port (used by E2E tests, manual
                // multi-instance runs, and the ADR-055 node topology
                // verification). Mirrors the HTTP port override.
                if let Ok(port_str) = std::env::var("ACOWORK_GATEWAY_MQTT_PORT")
                    && let Ok(port) = port_str.parse::<u16>()
                {
                    mqtt.port = port;
                }
                mqtt
            },
            advertise_host: cli
                .advertise_host
                .clone()
                .or(file_config.as_ref().and_then(|c| c.advertise_host.clone())),
            node_proxy_port: file_config.as_ref().and_then(|c| c.node_proxy_port),
            node_lsp_relay_port: file_config.as_ref().and_then(|c| c.node_lsp_relay_port),
            pm: file_config
                .as_ref()
                .map(|c| c.pm.clone())
                .unwrap_or_default(),
            doc: file_config
                .as_ref()
                .map(|c| c.doc.clone())
                .unwrap_or_default(),
        };

        config.validate()?;
        Ok(config)
    }

    /// Load config from a TOML file
    fn load_from_file(path: &str) -> Result<Option<Self>, GatewayError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            GatewayError::Config(format!("Failed to read config file '{}': {}", path, e))
        })?;
        let config: Self = toml::from_str(&content).map_err(|e| {
            GatewayError::Config(format!("Failed to parse config file '{}': {}", path, e))
        })?;
        Ok(Some(config))
    }

    /// Default config file path
    pub fn default_config_path() -> Result<std::path::PathBuf, GatewayError> {
        let base_dir = Self::project_config_dir();
        Ok(base_dir.join("gateway.toml"))
    }

    /// ADR-055 §3.2: the package registry — where uploaded `.agent`
    /// source files are stored for distribution to nodes. The Node
    /// downloads them via `GET /api/packages/{agent_id}/download`.
    ///
    /// Lives under `{data_dir}/package-registry` (a Gateway-private
    /// source-of-truth copy; the install directory proper belongs to
    /// the node).
    pub fn package_registry_dir(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.data_dir).join("package-registry")
    }

    /// Ensure required directories exist
    pub fn ensure_dirs(&self) -> Result<(), GatewayError> {
        let registry = self.package_registry_dir();
        for dir in [&self.vault_dir, &self.packages_dir, &self.data_dir] {
            std::fs::create_dir_all(dir).map_err(GatewayError::Io)?;
        }
        std::fs::create_dir_all(&registry).map_err(GatewayError::Io)?;
        Ok(())
    }

    /// Validate startup-sensitive configuration values.
    pub fn validate(&self) -> Result<(), GatewayError> {
        acowork_core::timeout_config::validate(&self.timeouts).map_err(GatewayError::Config)
    }

    /// Persist the current configuration to its source TOML file.
    /// Falls back to `default_config_path()` if `config_source_path` is not set.
    pub fn save(&self) -> Result<(), GatewayError> {
        let path = self
            .config_source_path
            .as_ref()
            .map(std::path::PathBuf::from)
            .or_else(|| Self::default_config_path().ok())
            .ok_or_else(|| GatewayError::Config("Cannot determine config file path".to_string()))?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(GatewayError::Io)?;
        }

        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| GatewayError::Config(format!("Failed to serialize config: {}", e)))?;

        std::fs::write(&path, &toml_str).map_err(|e| {
            GatewayError::Config(format!(
                "Failed to write config to '{}': {}",
                path.display(),
                e
            ))
        })?;

        tracing::info!(path = %path.display(), "Configuration persisted");
        Ok(())
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let base_dir = Self::project_config_dir();
        let data_dir = Self::project_data_dir();

        Self {
            config_source_path: None,
            vault_dir: base_dir.join("vault").to_string_lossy().to_string(),
            packages_dir: base_dir.join("packages").to_string_lossy().to_string(),
            data_dir: data_dir.to_string_lossy().to_string(),
            log_level: default_log_level(),
            log_file_size_mb: default_log_file_size_mb(),
            log_file_count: default_log_file_count(),
            timeouts: Timeouts::default(),
            max_iterations: default_max_iterations(),
            dev_mode: true,
            http: HttpConfig::default(),
            default_provider: None,
            default_model: None,
            max_output_tokens_limit: default_max_output_tokens_limit(),
            hf_mirrors: Vec::new(),
            embedding_model: None,
            data_flow: DataFlowConfig::default(),
            mqtt: Default::default(),
            advertise_host: None,
            node_proxy_port: None,
            node_lsp_relay_port: None,
            pm: PmConfig::default(),
            doc: DocConfig::default(),
        }
    }
}

/// Legacy XDG layout — `~/.config/acowork-gateway/`.
#[cfg(not(windows))]
fn legacy_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("acowork-gateway"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join("acowork-gateway"))
}

/// Legacy XDG layout — `~/.local/share/acowork-gateway/`.
#[cfg(not(windows))]
fn legacy_data_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("acowork-gateway"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| {
            PathBuf::from(h)
                .join(".local")
                .join("share")
                .join("acowork-gateway")
        })
}

/// Resolve the advertise host for this Gateway (ADR-055 D3 §6.3).
///
/// Priority: configured `advertise_host` > auto-detected first non-loopback
/// IPv4 > `"127.0.0.1"` (single-machine fallback). Logs a WARN whenever the
/// auto-detected value is used so operators know the address is not pinned.
pub(crate) fn resolve_advertise_host(config: &GatewayConfig) -> String {
    if let Some(h) = &config.advertise_host
        && !h.trim().is_empty()
    {
        return h.trim().to_string();
    }
    if let Some(ip) = detect_non_loopback_ip() {
        tracing::warn!(
            ip = %ip,
            "advertise_host not configured — auto-detected non-loopback IP. \
             Set [advertise_host] in gateway.toml (or --advertise-host) for \
             deterministic behavior across restarts (ADR-055 D3)."
        );
        return ip;
    }
    "127.0.0.1".to_string()
}

/// Best-effort detection of the first non-loopback IPv4 address on this
/// host. Uses the UDP "connect" trick: `connect` on a datagram socket
/// does not send any packets — it only asks the kernel to resolve the
/// route and assign the local source address, which we then read via
/// `local_addr`. Targets the well-known anycast `1.1.1.1`; no traffic
/// is emitted.
fn detect_non_loopback_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // connect on UDP does not emit packets; it only resolves the route
    // and pins the local source address for subsequent sends.
    socket.connect("1.1.1.1:80").ok()?;
    let addr = socket.local_addr().ok()?;
    match addr.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
            Some(v4.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::Mutex;

    /// Serialize tests that mutate `ACOWORK_HOME` to prevent flaky failures
    /// from parallel env-var races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_default_config() {
        let config = GatewayConfig::default();
        assert!(!config.vault_dir.is_empty());
        assert!(!config.packages_dir.is_empty());
        assert_eq!(config.log_level, "info");
        assert_eq!(config.max_iterations, 20);
        assert_eq!(config.timeouts.iteration_timeout_ms, 900_000);
        assert!(config.http.enabled);
        assert_eq!(config.http.port, 19876);
        assert_eq!(config.http.host, "127.0.0.1");
    }

    #[test]
    fn test_config_from_cli_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Use a temp home directory so no real config file is read.
        let tmp = std::env::temp_dir().join("acowork-test-config-defaults");
        let _ = std::fs::create_dir_all(&tmp);
        unsafe { std::env::set_var("ACOWORK_HOME", tmp.to_str().unwrap()) };
        unsafe { std::env::set_var("ACOWORK_GATEWAY_LOG_LEVEL", "info") };

        let cli = Cli::parse_from(["acowork-gateway"]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_config_from_cli_overrides() {
        let cli = Cli::parse_from([
            "acowork-gateway",
            "--log-level",
            "debug",
        ]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_ensure_dirs() {
        let config = GatewayConfig {
            config_source_path: None,
            vault_dir: format!("/tmp/test-gw-{}", std::process::id()),
            packages_dir: format!("/tmp/test-gw-pkg-{}", std::process::id()),
            data_dir: format!("/tmp/test-gw-data-{}", std::process::id()),
            dev_mode: false,
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        // Clean up
        let _ = std::fs::remove_dir_all(&config.vault_dir);
        let _ = std::fs::remove_dir_all(&config.packages_dir);
        let _ = std::fs::remove_dir_all(&config.data_dir);
    }

    #[test]
    fn test_project_root_default_layout() {
        let _lock = ENV_LOCK.lock().unwrap();
        // Clear override so we exercise the default path.
        // SAFETY: tests in this module run on a single test thread for
        // env-mutating work; concurrent tests don't touch ACOWORK_HOME.
        // (cargo test runs tests in parallel by default, so we accept
        // the small flake risk in exchange for keeping tests simple.)
        unsafe {
            std::env::remove_var("ACOWORK_HOME");
        }

        let cfg = GatewayConfig::default();
        let root = project_root();
        // config and data dirs must be siblings under the same root.
        assert!(cfg.vault_dir.starts_with(root.to_string_lossy().as_ref()));
        assert!(
            cfg.packages_dir
                .starts_with(root.to_string_lossy().as_ref())
        );
        // data_dir is its own sibling, not nested under config_dir.
        let root_str = root.to_string_lossy().to_string();
        assert!(
            cfg.data_dir.starts_with(&root_str),
            "data_dir should be under root ({root_str}), got {}",
            cfg.data_dir
        );
        // config/vault path: <root>/config/vault
        assert!(
            cfg.vault_dir.contains("/config/vault") || cfg.vault_dir.contains("\\config\\vault")
        );
        // data path: <root>/data
        assert!(cfg.data_dir.ends_with("/data") || cfg.data_dir.ends_with("\\data"));
    }

    #[test]
    fn test_project_root_respects_acowork_home() {
        let _lock = ENV_LOCK.lock().unwrap();
        // SAFETY: see comment in test_project_root_default_layout.
        unsafe {
            std::env::set_var("ACOWORK_HOME", "/tmp/acowork-home-test");
        }
        let root = project_root();
        assert_eq!(root, PathBuf::from("/tmp/acowork-home-test"));
        unsafe {
            std::env::remove_var("ACOWORK_HOME");
        }
    }

    // ── ADR-064: GatewayConfig.pm wiring ─────────────────────────────
    //
    // 覆盖 from_cli 解析 TOML `[pm]` 段。PM 作为独立进程，Gateway 侧
    // `[pm]` 只保留 enabled / port / auto_inject_mcp / mcp_http_path；
    // PM 自身调优参数（max_task_depth 等）由 acowork-pm 独立解析，不再
    // 嵌入 Gateway 配置树（`prepare_pm_data_dir` 已删除）。

    /// 临时写一份 TOML 配置文件,返回路径(测试结束由 caller 删除)。
    fn write_toml_config(label: &str, body: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acowork-test-pm-config-{label}-{}-{unique}.toml",
            std::process::id(),
        ));
        std::fs::write(&path, body).expect("write toml");
        path
    }

    /// `[pm]` 段被显式配置时,TOML 反序列化应正确读取所有字段 (ADR-064)。
    #[test]
    fn test_from_cli_reads_explicit_pm_section() {
        let _lock = ENV_LOCK.lock().unwrap();
        // 让 default_config_path() 找不到,避免污染
        let tmp_home = std::env::temp_dir().join("acowork-test-pm-config-explicit-home");
        let _ = std::fs::remove_dir_all(&tmp_home);
        std::fs::create_dir_all(&tmp_home).unwrap();
        unsafe { std::env::set_var("ACOWORK_HOME", tmp_home.to_str().unwrap()) };

        let toml = r#"
vault_dir = "/tmp/gw-explicit-test-vault"
packages_dir = "/tmp/gw-explicit-test-packages"
data_dir = "/tmp/gw-explicit-test-data"

[pm]
enabled = false
port = 19082
auto_inject_mcp = false
mcp_http_path = "/custom/pm/mcp"
"#;
        let path = write_toml_config("explicit", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert!(!config.pm.enabled);
        assert_eq!(config.pm.port, 19082);
        assert!(!config.pm.auto_inject_mcp);
        assert_eq!(config.pm.mcp_http_path, "/custom/pm/mcp");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&tmp_home);
    }

    /// `[pm]` 段被省略时,各字段走 `#[serde(default)]` (ADR-064)。
    #[test]
    fn test_from_cli_omitted_pm_section_uses_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = std::env::temp_dir().join("acowork-test-pm-config-omitted-home");
        let _ = std::fs::remove_dir_all(&tmp_home);
        std::fs::create_dir_all(&tmp_home).unwrap();
        unsafe { std::env::set_var("ACOWORK_HOME", tmp_home.to_str().unwrap()) };

        let toml = r#"
vault_dir = "/tmp/gw-omitted-test-vault"
packages_dir = "/tmp/gw-omitted-test-packages"
data_dir = "/tmp/gw-omitted-test-data"
"#;
        let path = write_toml_config("omitted", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert!(config.pm.enabled);
        assert_eq!(config.pm.port, 18082);
        assert!(config.pm.auto_inject_mcp);
        assert_eq!(config.pm.mcp_http_path, "/api/pm/mcp");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&tmp_home);
    }

    /// `[pm]` 段存在但只覆盖部分字段 —— serde(default) 应补齐未出现的字段。
    #[test]
    fn test_from_cli_partial_pm_section_uses_serde_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp_home = std::env::temp_dir().join("acowork-test-pm-config-partial-home");
        let _ = std::fs::remove_dir_all(&tmp_home);
        std::fs::create_dir_all(&tmp_home).unwrap();
        unsafe { std::env::set_var("ACOWORK_HOME", tmp_home.to_str().unwrap()) };

        let toml = r#"
vault_dir = "/tmp/gw-partial-test-vault"
packages_dir = "/tmp/gw-partial-test-packages"
data_dir = "/tmp/gw-partial-test-data"

[pm]
port = 20082
"#;
        let path = write_toml_config("partial", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert_eq!(config.pm.port, 20082);
        // 其他字段走默认
        assert!(config.pm.enabled);
        assert!(config.pm.auto_inject_mcp);
        assert_eq!(config.pm.mcp_http_path, "/api/pm/mcp");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&tmp_home);
    }

    #[test]
    fn test_doc_config_omitted_section_defaults() {
        // No `[doc]` section at all → all fields take defaults.
        let toml = r#"
vault_dir = "/tmp/gw-doc-default-test-vault"
packages_dir = "/tmp/gw-doc-default-test-packages"
data_dir = "/tmp/gw-doc-default-test-data"
"#;
        let path = write_toml_config("doc-default", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert!(config.doc.enabled);
        assert_eq!(config.doc.port, 18081);
        assert!(config.doc.data_dir.is_none());
        assert!(config.doc.auto_inject_mcp);
        assert_eq!(config.doc.mcp_http_path, "/api/doc/mcp");
        assert!(config.doc.request_ttl_hours.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_doc_config_partial_section_merges_defaults() {
        // Partial `[doc]` section → provided fields honored, rest defaulted.
        let toml = r#"
vault_dir = "/tmp/gw-doc-partial-test-vault"
packages_dir = "/tmp/gw-doc-partial-test-packages"
data_dir = "/tmp/gw-doc-partial-test-data"

[doc]
port = 20181
request_ttl_hours = 48
"#;
        let path = write_toml_config("doc-partial", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert_eq!(config.doc.port, 20181);
        assert_eq!(config.doc.request_ttl_hours, Some(48));
        // 其余走默认
        assert!(config.doc.enabled);
        assert!(config.doc.auto_inject_mcp);
        assert_eq!(config.doc.mcp_http_path, "/api/doc/mcp");
        assert!(config.doc.data_dir.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_doc_config_full_section() {
        // Fully-specified `[doc]` section round-trips through from_cli.
        let toml = r#"
vault_dir = "/tmp/gw-doc-full-test-vault"
packages_dir = "/tmp/gw-doc-full-test-packages"
data_dir = "/tmp/gw-doc-full-test-data"

[doc]
enabled = false
port = 20281
data_dir = "D:/tmp/doc-override"
auto_inject_mcp = false
mcp_http_path = "/api/doc/mcp-custom"
request_ttl_hours = 24
"#;
        let path = write_toml_config("doc-full", toml);
        let cli = Cli::parse_from(["acowork-gateway", "--config-path", path.to_str().unwrap()]);
        let config = GatewayConfig::from_cli(&cli).expect("from_cli");

        assert!(!config.doc.enabled);
        assert_eq!(config.doc.port, 20281);
        assert_eq!(
            config.doc.data_dir,
            Some(PathBuf::from("D:/tmp/doc-override"))
        );
        assert!(!config.doc.auto_inject_mcp);
        assert_eq!(config.doc.mcp_http_path, "/api/doc/mcp-custom");
        assert_eq!(config.doc.request_ttl_hours, Some(24));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_doc_config_default_struct() {
        // Direct Default impl sanity (unit-level, mirrors pm precedent).
        let doc = DocConfig::default();
        assert!(doc.enabled);
        assert_eq!(doc.port, 18081);
        assert!(doc.data_dir.is_none());
        assert!(doc.auto_inject_mcp);
        assert_eq!(doc.mcp_http_path, "/api/doc/mcp");
        assert!(doc.request_ttl_hours.is_none());
    }
}
