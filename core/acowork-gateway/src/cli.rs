//! Gateway CLI
//!
//! Supports daemon mode and CLI subcommands for package management,
//! agent lifecycle control, and listing.

use clap::{Parser, Subcommand};
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::gateway::Gateway;
use std::sync::Arc;

/// Global reference to the SizeRollingFileAppender for Gateway log file count
/// dynamic updates. Set by init_tracing() and read by update_log_file_count().
static FILE_APPENDER: std::sync::OnceLock<Arc<acowork_core::logging::SizeRollingFileAppender>> =
    std::sync::OnceLock::new();

/// Print ACowork logo with cyan (宝蓝) ANSI color on startup.
fn print_logo() {
    let color = "\x1b[36m";
    let reset = "\x1b[0m";
    let version = env!("CARGO_PKG_VERSION");
    let logo = format!(
        r#"{color}


 █████╗  ██████╗ ███████╗███╗   ██╗████████╗    ██████╗ ██████╗ ██╗    ██╗ ██████╗ ██████╗ ██╗  ██╗
██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝   ██╔════╝██╔═══██╗██║    ██║██╔═══██╗██╔══██╗██║ ██╔╝
███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║      ██║     ██║   ██║██║ █╗ ██║██║   ██║██████╔╝█████╔╝ 
██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║      ██║     ██║   ██║██║███╗██║██║   ██║██╔══██╗██╔═██╗ 
██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║      ╚██████╗╚██████╔╝╚███╔███╔╝╚██████╔╝██║  ██║██║  ██╗
╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝       ╚═════╝ ╚═════╝  ╚══╝╚══╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝
═══════════════════════════════════════════════════════════════════════════════════════════════════
{reset}{color}                                      Gateway{reset}
{color}                                      v{version}{reset}
"#,
        color = color,
        reset = reset,
        version = version
    );
    println!("{}", logo);
}

/// Gateway CLI
#[derive(Parser)]
#[command(name = "acowork-gateway")]
#[command(about = "ACowork Gateway - Agent lifecycle manager and IPC coordinator")]
#[command(version)]
pub struct Cli {
    /// Run as daemon (background service)
    #[arg(long, env = "ACOWORK_GATEWAY_DAEMON")]
    pub daemon: bool,

    /// Gateway socket path (overrides config)
    #[arg(long, env = "ACOWORK_GATEWAY_SOCKET_PATH")]
    pub socket_path: Option<String>,

    /// Vault directory (overrides config)
    #[arg(long, env = "ACOWORK_GATEWAY_VAULT_DIR")]
    pub vault_dir: Option<String>,

    /// Packages directory (overrides config)
    #[arg(long, env = "ACOWORK_GATEWAY_PACKAGES_DIR")]
    pub packages_dir: Option<String>,

    /// Config file path
    #[arg(long, env = "ACOWORK_GATEWAY_CONFIG")]
    pub config_path: Option<String>,

    /// Gateway home (application root) directory.
    ///
    /// Contains `config/` and `data/` subdirectories. Default:
    /// - Linux/macOS: `$HOME/.acowork/acowork-gateway/`
    /// - Windows:     `%USERPROFILE%\.acowork\acowork-gateway\`
    ///
    /// Overrides the `ACOWORK_HOME` environment variable. Useful for
    /// tests and running multiple isolated instances side-by-side.
    #[arg(long, env = "ACOWORK_HOME")]
    pub home: Option<String>,

    /// Log level (trace/debug/info/warn/error)
    #[arg(long, env = "ACOWORK_GATEWAY_LOG_LEVEL", default_value = "info")]
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
    /// Package an installed agent into .agent file
    Package {
        /// Agent ID to package
        agent_id: String,
        /// Output directory (default: ./build)
        #[arg(long, env = "ACOWORK_PACKAGE_OUTPUT")]
        output: Option<String>,
        /// Sign the package with developer key
        #[arg(long)]
        sign: bool,
        /// Signing key directory (default: examples/.signing-keys)
        #[arg(long, env = "ACOWORK_PACKAGE_KEY_DIR")]
        key_dir: Option<String>,
    },
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<(), GatewayError> {
        // Print ACowork logo with green color
        print_logo();

        // Apply --home flag to ACOWORK_HOME so config path resolution
        // picks it up consistently (CLI > env > default).
        if let Some(home) = &self.home
            && !home.is_empty() {
                // SAFETY: single-threaded at this point in startup.
                unsafe { std::env::set_var("ACOWORK_HOME", home); }
            }

        // One-time migration from the legacy split layout. Must run BEFORE
        // init_tracing creates the log dir, otherwise `new_root.exists()`
        // would be true (the freshly-created log dir counts) and migration
        // would skip. Uses eprintln! because tracing isn't set up yet.
        GatewayConfig::migrate_legacy_layout();

        // Load config (paths now reflect the migrated layout).
        let config = GatewayConfig::from_cli(&self)?;
        // Initialize tracing with reload support
        let log_reload_handle = init_tracing(&config.log_level, config.log_file_size_mb, config.log_file_count);

        // Install global panic hook AFTER tracing is initialized so panic
        // messages are captured in both stderr and the rolling log file.
        acowork_core::logging::install_panic_hook();

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
                    println!("ACowork Gateway — use subcommands or --daemon to start service");
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
/// Uses the shared [`acowork_core::crlf::CrlfWriter`] implementation.
struct CrlfStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CrlfStderr {
    type Writer = acowork_core::crlf::CrlfWriter<std::io::Stderr>;

    fn make_writer(&self) -> Self::Writer {
        acowork_core::crlf::CrlfWriter {
            inner: std::io::stderr(),
        }
    }
}
///
/// Logs are written to stderr AND to `<root>/data/logs/gateway-<timestamp>.log`
/// (sibling of `embed.log`; both gateway and embed logs live under `data/logs/`).
///
/// Returns the reload handle so the Gateway can dynamically change
/// the log level at runtime via the HTTP config API.
fn init_tracing(level: &str, log_file_size_mb: u64, log_file_count: u64) -> Option<crate::LogReloadHandle> {
    use tracing_subscriber::{reload, EnvFilter, layer::SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use acowork_core::logging::ChronoLocalTimer;
    use crate::config::GatewayConfig;

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    // Log directory: <root>/data/logs/  (sibling of embed.log)
    let log_dir = GatewayConfig::project_data_dir().join("logs");

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
                    .with_timer(ChronoLocalTimer)
                    .compact()
            )
            .init();
        return Some(handle);
    }

    // Size-based rolling file appender: splits when file exceeds log_file_size_mb
    let max_file_count = if log_file_count > 0 { log_file_count as usize } else { 0 };
    let file_appender = Arc::new(acowork_core::logging::SizeRollingFileAppender::new(
        log_dir,
        if log_file_size_mb > 0 { log_file_size_mb } else { 10 },
        max_file_count,
    ));
    // Store for dynamic log file count updates
    let _ = FILE_APPENDER.set(file_appender.clone());
    let (filter, handle) = reload::Layer::new(env_filter);

    // Stderr layer (for terminal output, compact format, no colors)
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(CrlfStderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_ansi(false)
        .with_timer(ChronoLocalTimer)
        .compact();

    // File layer (file_appender implements MakeWriter, writes \n line endings
    // which most modern editors handle; for Notepad, use a proper text editor)
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_ansi(false)
        .with_timer(ChronoLocalTimer);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Some(handle)
}

/// Dynamically update the Gateway's own log file maximum count.
/// Immediately enforces the limit by deleting the oldest files
/// from the Gateway log directory.
pub(crate) fn update_log_file_count(count: u64) {
    if let Some(appender) = FILE_APPENDER.get() {
        let max = if count > 0 { count as usize } else { 0 };
        appender.set_max_file_count(max);
        tracing::info!(log_file_count = count, "Gateway log file count updated dynamically");
    }
}

// Re-export from acowork-core (shared with Agent Runtime)

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
        let cli = Cli::parse_from(["acowork-gateway", "--daemon"]);
        assert!(cli.daemon);
    }

    #[test]
    fn test_cli_parse_install() {
        let cli = Cli::parse_from(["acowork-gateway", "install", "weather.agent"]);
        match cli.command {
            Some(Commands::Install { package }) => {
                assert_eq!(package, "weather.agent");
            }
            _ => panic!("Expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_start() {
        let cli = Cli::parse_from(["acowork-gateway", "start", "com.example.weather"]);
        match cli.command {
            Some(Commands::Start { agent_id }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Start command"),
        }
    }

    #[test]
    fn test_cli_parse_stop() {
        let cli = Cli::parse_from(["acowork-gateway", "stop", "com.example.weather"]);
        match cli.command {
            Some(Commands::Stop { agent_id }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Stop command"),
        }
    }

    #[test]
    fn test_cli_parse_list() {
        let cli = Cli::parse_from(["acowork-gateway", "list"]);
        match cli.command {
            Some(Commands::List) => {}
            _ => panic!("Expected List command"),
        }
    }

    #[test]
    fn test_cli_parse_upgrade() {
        let cli = Cli::parse_from([
            "acowork-gateway",
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
        let cli = Cli::parse_from(["acowork-gateway"]);
        assert_eq!(cli.log_level, "info");
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::parse_from(["acowork-gateway", "--log-level", "debug"]);
        assert_eq!(cli.log_level, "debug");
    }
}
