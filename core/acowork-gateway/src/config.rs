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
use acowork_core::defaults;
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
        && !p.is_empty() {
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
    /// Socket path for IPC (Unix Socket on Linux, Named Pipe on Windows)
    pub socket_path: String,
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
    /// Default idle timeout in seconds (0 = no timeout)
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Default max iterations per agent run
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Default iteration timeout in milliseconds
    #[serde(default = "default_iteration_timeout_ms")]
    pub iteration_timeout_ms: u64,
    /// Development mode: allows unsigned packages, relaxed security
    #[serde(default)]
    pub dev_mode: bool,
    /// HTTP API configuration
    #[serde(default)]
    pub http: HttpConfig,
    /// Default LLM provider for agents
    /// When set, Gateway delivers this provider's config to agents via IPC.
    /// If not set, falls back to the first key stored in Vault.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default LLM model for agents
    /// When set, Gateway delivers this model to agents via IPC.
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
    /// LSP config directory (contains lsp_servers.json and lsp_install/).
    ///
    /// In local mode (Desktop App), this is the Tauri resource_dir where
    /// LSP config files are bundled. In remote mode (standalone Gateway),
    /// this is unset and Gateway falls back to scanning exe_dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_config_dir: Option<String>,
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
    /// Enable CORS for Desktop App
    #[serde(default)]
    pub cors_enabled: bool,
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
fn default_http_port() -> u16 {
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
            cors_enabled: false,
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
fn default_idle_timeout() -> u64 {
    300
} // 5 minutes
fn default_max_iterations() -> u32 {
    20
}
fn default_iteration_timeout_ms() -> u64 {
    30_000
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
        if new_root.exists() {
        }

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

            if let Some(old) = legacy_config_dir() {
                if old.exists() {
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
            }

            if let Some(old) = legacy_data_dir() {
                if old.exists() {
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
        let default_socket = if cfg!(windows) {
            r"\\.\pipe\acowork-gateway".to_string()
        } else {
            base_dir.join("gateway.sock").to_string_lossy().to_string()
        };
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
        Ok(Self {
            config_source_path: config_path,
            socket_path: cli
                .socket_path
                .clone()
                .or(file_config.as_ref().map(|c| c.socket_path.clone()))
                .unwrap_or(default_socket),
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
            idle_timeout_secs: file_config
                .as_ref()
                .map(|c| c.idle_timeout_secs)
                .unwrap_or_else(default_idle_timeout),
            max_iterations: file_config
                .as_ref()
                .map(|c| c.max_iterations)
                .unwrap_or_else(default_max_iterations),
            iteration_timeout_ms: file_config
                .as_ref()
                .map(|c| c.iteration_timeout_ms)
                .unwrap_or_else(default_iteration_timeout_ms),
            dev_mode: file_config.as_ref().map(|c| c.dev_mode).unwrap_or(true),
            http: {
                let mut http = file_config
                    .as_ref()
                    .map(|c| c.http.clone())
                    .unwrap_or_default();
                // Allow ACOWORK_GATEWAY_HTTP_PORT env var to override the
                // configured port (used by E2E tests and manual testing).
                if let Ok(port_str) = std::env::var("ACOWORK_GATEWAY_HTTP_PORT") {
                    if let Ok(port) = port_str.parse::<u16>() {
                        http.port = port;
                        // Ensure port_max is at least port+10 to allow
                        // auto-increment on conflict.
                        if http.port_max < port + 10 {
                            http.port_max = port + 10;
                        }
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
            lsp_config_dir: cli
                .lsp_config_dir
                .clone()
                .or_else(|| file_config.as_ref().and_then(|c| c.lsp_config_dir.clone())),
        })
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

    /// Ensure required directories exist
    pub fn ensure_dirs(&self) -> Result<(), GatewayError> {
        for dir in [&self.vault_dir, &self.packages_dir, &self.data_dir] {
            std::fs::create_dir_all(dir).map_err(GatewayError::Io)?;
        }
        Ok(())
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

        let default_socket = if cfg!(windows) {
            r"\\.\pipe\acowork-gateway".to_string()
        } else {
            base_dir.join("gateway.sock").to_string_lossy().to_string()
        };

        Self {
            config_source_path: None,
            socket_path: default_socket,
            vault_dir: base_dir.join("vault").to_string_lossy().to_string(),
            packages_dir: base_dir.join("packages").to_string_lossy().to_string(),
            data_dir: data_dir.to_string_lossy().to_string(),
            log_level: default_log_level(),
            log_file_size_mb: default_log_file_size_mb(),
            log_file_count: default_log_file_count(),
            idle_timeout_secs: default_idle_timeout(),
            max_iterations: default_max_iterations(),
            iteration_timeout_ms: default_iteration_timeout_ms(),
            dev_mode: true,
            http: HttpConfig::default(),
            default_provider: None,
            default_model: None,
            max_output_tokens_limit: default_max_output_tokens_limit(),
            hf_mirrors: Vec::new(),
            embedding_model: None,
            lsp_config_dir: None,
        }
    }
}

/// Legacy XDG layout — `~/.config/acowork-gateway/`.
#[cfg(not(windows))]
fn legacy_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("acowork-gateway"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join("acowork-gateway"))
}

/// Legacy XDG layout — `~/.local/share/acowork-gateway/`.
#[cfg(not(windows))]
fn legacy_data_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("acowork-gateway"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_default_config() {
        let config = GatewayConfig::default();
        assert!(!config.socket_path.is_empty());
        assert!(!config.vault_dir.is_empty());
        assert!(!config.packages_dir.is_empty());
        assert_eq!(config.log_level, "info");
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.max_iterations, 20);
        assert_eq!(config.iteration_timeout_ms, 30_000);
        assert!(config.http.enabled);
        assert_eq!(config.http.port, 19876);
        assert_eq!(config.http.host, "127.0.0.1");
    }

    #[test]
    fn test_config_from_cli_defaults() {
        let cli = Cli::parse_from(["acowork-gateway"]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert!(!config.socket_path.is_empty());
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_config_from_cli_overrides() {
        let cli = Cli::parse_from([
            "acowork-gateway",
            "--socket-path",
            "/tmp/custom.sock",
            "--log-level",
            "debug",
        ]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert_eq!(config.socket_path, "/tmp/custom.sock");
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_ensure_dirs() {
        let config = GatewayConfig {
            config_source_path: None,
            socket_path: "/tmp/test-gw/gateway.sock".to_string(),
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
}
