//! Gateway CLI
//!
//! Supports daemon mode and CLI subcommands for package management,
//! agent lifecycle control, and listing.

use clap::{Parser, Subcommand};
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::gateway::Gateway;

/// Gateway CLI
#[derive(Parser)]
#[command(name = "rollball-gateway")]
#[command(about = "RollBall Gateway - Agent lifecycle manager and IPC coordinator")]
#[command(version)]
pub struct Cli {
    /// Run as daemon (background service)
    #[arg(long, env = "ROLLBALL_GATEWAY_DAEMON")]
    pub daemon: bool,

    /// Gateway socket path (overrides config)
    #[arg(long, env = "ROLLBALL_GATEWAY_SOCKET_PATH")]
    pub socket_path: Option<String>,

    /// Vault directory (overrides config)
    #[arg(long, env = "ROLLBALL_GATEWAY_VAULT_DIR")]
    pub vault_dir: Option<String>,

    /// Packages directory (overrides config)
    #[arg(long, env = "ROLLBALL_GATEWAY_PACKAGES_DIR")]
    pub packages_dir: Option<String>,

    /// Config file path
    #[arg(long, env = "ROLLBALL_GATEWAY_CONFIG")]
    pub config_path: Option<String>,

    /// Log level (trace/debug/info/warn/error)
    #[arg(long, env = "ROLLBALL_GATEWAY_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Subcommands
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Install a .agent package
    Install {
        /// Path to .agent package file
        package: String,
    },
    /// Uninstall an agent
    Uninstall {
        /// Agent ID to uninstall
        agent_id: String,
    },
    /// Upgrade an installed agent
    Upgrade {
        /// Agent ID to upgrade
        agent_id: String,
        /// Path to new .agent package file
        package: String,
    },
    /// Start an agent
    Start {
        /// Agent ID to start
        agent_id: String,
    },
    /// Stop a running agent
    Stop {
        /// Agent ID to stop
        agent_id: String,
    },
    /// List installed agents
    List,
    /// Manage agent permissions
    Permission {
        /// Subcommand: revoke, reset, list
        #[command(subcommand)]
        action: PermissionAction,
    },
    /// Package an installed agent into .agent file
    Package {
        /// Agent ID to package
        agent_id: String,
        /// Output directory (default: ./build)
        #[arg(long, env = "ROLLBALL_PACKAGE_OUTPUT")]
        output: Option<String>,
        /// Sign the package with developer key
        #[arg(long)]
        sign: bool,
        /// Signing key directory (default: examples/.signing-keys)
        #[arg(long, env = "ROLLBALL_PACKAGE_KEY_DIR")]
        key_dir: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PermissionAction {
    /// Revoke a specific permission from an agent
    Revoke {
        /// Agent ID
        agent_id: String,
        /// Permission string (e.g., "shell", "network:https://api.example.com")
        permission: String,
    },
    /// Reset all permissions for an agent
    Reset {
        /// Agent ID
        agent_id: String,
    },
    /// List granted permissions for an agent
    List {
        /// Agent ID
        agent_id: String,
    },
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<(), GatewayError> {
        // Initialize tracing with reload support (daemon mode only needs the handle)
        let log_reload_handle = init_tracing(&self.log_level);

        let config = GatewayConfig::from_cli(&self)?;
        let mut gateway = Gateway::new(config)?;

        match self.command {
            Some(Commands::Install { package }) => {
                let msg = gateway.install_package(&package)?;
                println!("{}", msg);
            }
            Some(Commands::Uninstall { agent_id }) => {
                let msg = gateway.uninstall_package(&agent_id)?;
                println!("{}", msg);
            }
            Some(Commands::Upgrade { agent_id, package }) => {
                let msg = gateway.upgrade_package(&agent_id, &package)?;
                println!("{}", msg);
            }
            Some(Commands::Start { agent_id }) => {
                // Need async runtime for start/stop
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let msg = rt.block_on(gateway.start_agent(&agent_id))?;
                println!("{}", msg);
            }
            Some(Commands::Stop { agent_id }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let msg = rt.block_on(gateway.stop_agent(&agent_id))?;
                println!("{}", msg);
            }
            Some(Commands::List) => {
                let entries = gateway.list_agents();
                if entries.is_empty() {
                    println!("No agents installed.");
                } else {
                    for entry in entries {
                        println!("  {}", entry);
                    }
                }
            }
            Some(Commands::Permission { action }) => {
                let msg = gateway.handle_permission_cli(action)?;
                println!("{}", msg);
            }
            Some(Commands::Package { agent_id, output, sign, key_dir }) => {
                let msg = gateway.package_agent(&agent_id, output.as_deref(), sign, key_dir.as_deref())?;
                println!("{}", msg);
            }
            None => {
                if self.daemon {
                    tracing::info!("Starting gateway in daemon mode");
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(4)
                        .enable_all()
                        .build()
                        .map_err(GatewayError::Io)?;
                    rt.block_on(async_main(gateway, log_reload_handle))?;
                } else {
                    // No subcommand and no daemon flag — show help
                    println!("RollBall Gateway — use subcommands or --daemon to start service");
                    println!("Run with --help for usage information");
                }
            }
        }
        Ok(())
    }
}

/// Cross-platform CRLF conversion for terminal log output.
///
/// On Windows, Rust's `io::Stderr` writes `\n` byte-for-byte, producing
/// unix-style line endings.  This wrapper inserts `\r` before each `\n`.
/// On Unix it is a transparent pass-through.
///
/// Uses the shared [`rollball_core::crlf::CrlfWriter`] implementation.
struct CrlfStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CrlfStderr {
    type Writer = rollball_core::crlf::CrlfWriter<std::io::Stderr>;

    fn make_writer(&self) -> Self::Writer {
        rollball_core::crlf::CrlfWriter {
            inner: std::io::stderr(),
        }
    }
}
///
/// Logs are written to stderr AND to `~/.config/rollball-gateway/logs/gateway.log`.
///
/// Returns the reload handle so the Gateway can dynamically change
/// the log level at runtime via the HTTP config API.
fn init_tracing(level: &str) -> Option<crate::LogReloadHandle> {
    use tracing_subscriber::{reload, EnvFilter, layer::SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::fmt::time::LocalTime;
    use crate::config::GatewayConfig;

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    // Log directory: ~/.config/rollball-gateway/logs/
    let log_dir = GatewayConfig::project_config_dir().join("logs");

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "WARN: failed to create log directory {:?}: {}; falling back to stderr-only",
            log_dir, e
        );
        // Fallback: stderr-only with reload support
        let (filter, handle) = reload::Layer::new(env_filter);
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(CrlfStderr)
                    .with_target(false)
                    .with_timer(LocalTime::rfc_3339())
                    .compact()
            )
            .init();
        return Some(handle);
    }

    // File appender for persistent logging
    let file_appender = tracing_appender::rolling::never(&log_dir, "gateway.log");
    let (filter, handle) = reload::Layer::new(env_filter);

    // Stderr layer (for terminal output, compact format, no colors)
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(CrlfStderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_ansi(false)
        .with_timer(LocalTime::rfc_3339())
        .compact();

    // File layer (file_appender implements MakeWriter, writes \n line endings
    // which most modern editors handle; for Notepad, use a proper text editor)
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_ansi(false)
        .with_timer(LocalTime::rfc_3339());

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Some(handle)
}

/// Async main entry point for daemon mode
async fn async_main(
    mut gateway: Gateway,
    log_reload_handle: Option<crate::LogReloadHandle>,
) -> Result<(), GatewayError> {
    tracing::info!("Gateway daemon starting");

    // Run the gateway event loop
    gateway.run(log_reload_handle).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parse_daemon() {
        let cli = Cli::parse_from(["rollball-gateway", "--daemon"]);
        assert!(cli.daemon);
    }

    #[test]
    fn test_cli_parse_install() {
        let cli = Cli::parse_from(["rollball-gateway", "install", "weather.agent"]);
        match cli.command {
            Some(Commands::Install { package }) => {
                assert_eq!(package, "weather.agent");
            }
            _ => panic!("Expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_start() {
        let cli = Cli::parse_from(["rollball-gateway", "start", "com.example.weather"]);
        match cli.command {
            Some(Commands::Start { agent_id }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Start command"),
        }
    }

    #[test]
    fn test_cli_parse_stop() {
        let cli = Cli::parse_from(["rollball-gateway", "stop", "com.example.weather"]);
        match cli.command {
            Some(Commands::Stop { agent_id }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Stop command"),
        }
    }

    #[test]
    fn test_cli_parse_list() {
        let cli = Cli::parse_from(["rollball-gateway", "list"]);
        match cli.command {
            Some(Commands::List) => {}
            _ => panic!("Expected List command"),
        }
    }

    #[test]
    fn test_cli_parse_upgrade() {
        let cli = Cli::parse_from([
            "rollball-gateway",
            "upgrade",
            "com.example.weather",
            "weather-v2.agent",
        ]);
        match cli.command {
            Some(Commands::Upgrade { agent_id, package }) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(package, "weather-v2.agent");
            }
            _ => panic!("Expected Upgrade command"),
        }
    }

    #[test]
    fn test_cli_default_log_level() {
        let cli = Cli::parse_from(["rollball-gateway"]);
        assert_eq!(cli.log_level, "info");
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::parse_from(["rollball-gateway", "--log-level", "debug"]);
        assert_eq!(cli.log_level, "debug");
    }
}
