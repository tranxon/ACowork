//! Gateway configuration
//!
//! Configuration can come from:
//! 1. CLI arguments (highest priority)
//! 2. Environment variables
//! 3. Config file (gateway.toml)
//! 4. Defaults (lowest priority)

use serde::{Deserialize, Serialize};
use directories::ProjectDirs;
use crate::cli::Cli;
use crate::error::GatewayError;

/// Gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
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
}

fn default_log_level() -> String { "info".to_string() }
fn default_idle_timeout() -> u64 { 300 } // 5 minutes
fn default_max_iterations() -> u32 { 20 }
fn default_iteration_timeout_ms() -> u64 { 30_000 }

impl GatewayConfig {
    /// Get the project config directory
    fn project_config_dir() -> std::path::PathBuf {
        ProjectDirs::from("com", "rollball", "rollball-gateway")
            .map(|pd| pd.config_dir().to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(".").join(".rollball-gateway"))
    }

    /// Get the project data directory
    fn project_data_dir() -> std::path::PathBuf {
        ProjectDirs::from("com", "rollball", "rollball-gateway")
            .map(|pd| pd.data_dir().to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(".").join(".rollball-gateway-data"))
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
        let default_socket = base_dir.join("gateway.sock")
            .to_string_lossy().to_string();
        let default_vault = base_dir.join("vault")
            .to_string_lossy().to_string();
        let default_packages = base_dir.join("packages")
            .to_string_lossy().to_string();

        let data_dir = Self::project_data_dir();
        let default_data = data_dir.to_string_lossy().to_string();

        // Merge: CLI > env > file > defaults
        Ok(Self {
            socket_path: cli.socket_path.clone()
                .or(file_config.as_ref().map(|c| c.socket_path.clone()))
                .unwrap_or(default_socket),
            vault_dir: cli.vault_dir.clone()
                .or(file_config.as_ref().map(|c| c.vault_dir.clone()))
                .unwrap_or(default_vault),
            packages_dir: cli.packages_dir.clone()
                .or(file_config.as_ref().map(|c| c.packages_dir.clone()))
                .unwrap_or(default_packages),
            data_dir: file_config.as_ref().map(|c| c.data_dir.clone())
                .unwrap_or(default_data),
            log_level: if cli.log_level != "info" {
                cli.log_level.clone()
            } else {
                file_config.as_ref().map(|c| c.log_level.clone())
                    .unwrap_or_else(default_log_level)
            },
            idle_timeout_secs: file_config.as_ref().map(|c| c.idle_timeout_secs)
                .unwrap_or_else(default_idle_timeout),
            max_iterations: file_config.as_ref().map(|c| c.max_iterations)
                .unwrap_or_else(default_max_iterations),
            iteration_timeout_ms: file_config.as_ref().map(|c| c.iteration_timeout_ms)
                .unwrap_or_else(default_iteration_timeout_ms),
            dev_mode: file_config.as_ref().map(|c| c.dev_mode).unwrap_or(false),
        })
    }

    /// Load config from a TOML file
    fn load_from_file(path: &str) -> Result<Option<Self>, GatewayError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| GatewayError::Config(format!("Failed to read config file '{}': {}", path, e)))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| GatewayError::Config(format!("Failed to parse config file '{}': {}", path, e)))?;
        Ok(Some(config))
    }

    /// Default config file path
    fn default_config_path() -> Result<std::path::PathBuf, GatewayError> {
        let base_dir = Self::project_config_dir();
        Ok(base_dir.join("gateway.toml"))
    }

    /// Ensure required directories exist
    pub fn ensure_dirs(&self) -> Result<(), GatewayError> {
        for dir in [&self.vault_dir, &self.packages_dir, &self.data_dir] {
            std::fs::create_dir_all(dir)
                .map_err(GatewayError::Io)?;
        }
        Ok(())
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let base_dir = Self::project_config_dir();
        let data_dir = Self::project_data_dir();

        Self {
            socket_path: base_dir.join("gateway.sock").to_string_lossy().to_string(),
            vault_dir: base_dir.join("vault").to_string_lossy().to_string(),
            packages_dir: base_dir.join("packages").to_string_lossy().to_string(),
            data_dir: data_dir.to_string_lossy().to_string(),
            log_level: default_log_level(),
            idle_timeout_secs: default_idle_timeout(),
            max_iterations: default_max_iterations(),
            iteration_timeout_ms: default_iteration_timeout_ms(),
            dev_mode: false,
        }
    }
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
    }

    #[test]
    fn test_config_from_cli_defaults() {
        let cli = Cli::parse_from(["rollball-gateway"]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert!(!config.socket_path.is_empty());
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn test_config_from_cli_overrides() {
        let cli = Cli::parse_from([
            "rollball-gateway",
            "--socket-path", "/tmp/custom.sock",
            "--log-level", "debug",
        ]);
        let config = GatewayConfig::from_cli(&cli).unwrap();
        assert_eq!(config.socket_path, "/tmp/custom.sock");
        assert_eq!(config.log_level, "debug");
    }

    #[test]
    fn test_ensure_dirs() {
        let config = GatewayConfig {
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
}
