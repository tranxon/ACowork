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
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<(), GatewayError> {
        // Initialize tracing
        init_tracing(&self.log_level);

        let config = GatewayConfig::from_cli(&self)?;
        let mut gateway = Gateway::new(config);

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
                let rt = tokio::runtime::Runtime::new()
                    .map_err(GatewayError::Io)?;
                let msg = rt.block_on(gateway.start_agent(&agent_id))?;
                println!("{}", msg);
            }
            Some(Commands::Stop { agent_id }) => {
                let rt = tokio::runtime::Runtime::new()
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
            None => {
                if self.daemon {
                    tracing::info!("Starting gateway in daemon mode");
                    let rt = tokio::runtime::Runtime::new()
                        .map_err(GatewayError::Io)?;
                    rt.block_on(async_main(gateway))?;
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

/// Initialize tracing subscriber
fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Async main entry point for daemon mode
async fn async_main(mut gateway: Gateway) -> Result<(), GatewayError> {
    tracing::info!("Gateway daemon starting");

    // Run the gateway event loop
    gateway.run().await?;

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
