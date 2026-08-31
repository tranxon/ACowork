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


 █████╗   ██████╗ ██████╗ ██╗    ██╗ ██████╗ ██████╗ ██╗  ██╗
██╔══██╗ ██╔════╝██╔═══██╗██║    ██║██╔═══██╗██╔══██╗██║ ██╔╝
███████║ ██║     ██║   ██║██║ █╗ ██║██║   ██║██████╔╝█████╔╝ 
██╔══██║ ██║     ██║   ██║██║███╗██║██║   ██║██╔══██╗██╔═██╗ 
██║  ██║ ╚██████╗╚██████╔╝╚███╔███╔╝╚██████╔╝██║  ██║██║  ██╗
╚═╝  ╚═╝  ╚═════╝ ╚═════╝  ╚══╝╚══╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝
═════════════════════════════════════════════════════════════
{reset}{color}                           Gateway{reset}
{color}                           v{version}{reset}
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
#[command(about = "ACowork Gateway - Agent lifecycle manager and gRPC coordinator")]
#[command(version)]
pub struct Cli {
    /// Run as daemon (background service)
    #[arg(long, env = "ACOWORK_GATEWAY_DAEMON")]
    pub daemon: bool,

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

    /// Advertise host: the address other machines should use to reach
    /// services on this Gateway (embed / LSP / broker endpoints
    /// distributed via MQTT). Distinct from the bind address.
    /// If unset, Gateway auto-detects the first non-loopback IP and
    /// warns; defaults to 127.0.0.1 when detection fails.
    /// (ADR-055 D3 §6.3)
    #[arg(long, env = "ACOWORK_GATEWAY_ADVERTISE_HOST")]
    pub advertise_host: Option<String>,

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
        /// Target node (ADR-055 §6.13.3; default `local`). Wiring
        /// lands in Phase 3 — the command is currently delegated to
        /// the local node via the HTTP API.
        #[arg(long, default_value = "local")]
        node: String,
    },
    /// Uninstall an agent
    Uninstall {
        /// Agent ID to uninstall
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default `local`).
        #[arg(long, default_value = "local")]
        node: String,
    },
    /// Upgrade an installed agent
    Upgrade {
        /// Agent ID to upgrade
        agent_id: String,
        /// Path to new .agent package file
        package: String,
        /// Target node (ADR-055 §6.13.3; default `local`).
        #[arg(long, default_value = "local")]
        node: String,
    },
    /// Start an agent
    Start {
        /// Agent ID to start
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default `local`).
        #[arg(long, default_value = "local")]
        node: String,
    },
    /// Stop a running agent
    Stop {
        /// Agent ID to stop
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default `local`).
        #[arg(long, default_value = "local")]
        node: String,
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
    /// Node Agent management (ADR-055)
    Nodes {
        #[command(subcommand)]
        cmd: NodesCommands,
    },
}

/// Node Agent subcommands (ADR-055 §6.13.3).
#[derive(Subcommand)]
pub enum NodesCommands {
    /// List known Node Agents (queries the running Gateway's MQTT
    /// broker for retained status/info topics).
    List,
    /// Stop every agent running on a node (migration precursor).
    Drain {
        /// Target node id.
        node_id: String,
    },
    /// Remove a node's records (requires the node to be offline).
    Remove {
        /// Target node id.
        node_id: String,
    },
    /// Generate an enrollment token (Phase 5a placeholder).
    Token {
        #[command(subcommand)]
        cmd: TokenCommands,
    },
}

/// Enrollment-token subcommands (ADR-055 §6.13.3 — Phase 5a).
#[derive(Subcommand)]
pub enum TokenCommands {
    /// Create an enrollment token.
    Create {
        /// Token lifetime (default 30m).
        #[arg(long, default_value = "30m")]
        ttl: String,
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

        // Extract data_flow config before config is moved into Gateway::new
        let worker_threads = config.data_flow.worker_threads;
        // ADR-055: capture MQTT broker address before config moves, for
        // the `nodes list` CLI query (it talks to the running daemon's
        // broker directly, no daemon state).
        let mqtt_host = config.mqtt.host.clone();
        let mqtt_port = config.mqtt.port;
        // ADR-055 §6.13.3: capture the pieces the install/upgrade CLI
        // commands need to spool packages + build the registry download
        // URL (all before config moves into Gateway::new).
        let registry_dir = config.package_registry_dir();
        let advertise_host = crate::config::resolve_advertise_host(&config);
        let http_host = config.http.host.clone();
        let http_port = config.http.port;
        let dev_mode = config.dev_mode;
        // ADR-055 Phase 5a: data dir is needed by `nodes token create`
        // (the enrollment token store lives under it) — captured before
        // config moves into Gateway::new.
        let data_dir = config.data_dir.clone();
        let gateway = Gateway::new(config)?;
        match self.command {
            Some(Commands::Install { package, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let dispatch = crate::gateway::node_manager::CliPackageDispatch {
                    mqtt_host: &mqtt_host,
                    mqtt_port,
                    node_id: &node,
                    registry_dir: &registry_dir,
                    advertise_host: &advertise_host,
                    http_host: &http_host,
                    http_port,
                    dev_mode,
                };
                rt.block_on(crate::gateway::node_manager::install_agent_via_mqtt(
                    &dispatch,
                    std::path::Path::new(&package),
                ))?;
            }
            Some(Commands::Uninstall { agent_id, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::uninstall_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::Upgrade { agent_id, package, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let dispatch = crate::gateway::node_manager::CliPackageDispatch {
                    mqtt_host: &mqtt_host,
                    mqtt_port,
                    node_id: &node,
                    registry_dir: &registry_dir,
                    advertise_host: &advertise_host,
                    http_host: &http_host,
                    http_port,
                    dev_mode,
                };
                rt.block_on(crate::gateway::node_manager::upgrade_agent_via_mqtt(
                    &dispatch,
                    &agent_id,
                    std::path::Path::new(&package),
                ))?;
            }
            Some(Commands::Start { agent_id, node }) => {
                // Need async runtime for start/stop
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::start_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::Stop { agent_id, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::stop_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::List) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let entries = rt.block_on(gateway.list_agents());
                if entries.is_empty() {
                    println!("No agents installed.");
                } else {
                    for entry in entries {
                        println!("  {}", entry);
                    }
                }
            }
            Some(Commands::Package { agent_id, output, sign, key_dir }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let msg = rt.block_on(gateway.package_agent(&agent_id, output.as_deref(), sign, key_dir.as_deref()))?;
                println!("{}", msg);
            }
            Some(Commands::Nodes { cmd: NodesCommands::List }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::list_nodes_via_mqtt(
                    &mqtt_host, mqtt_port,
                ))?;
            }
            Some(Commands::Nodes { cmd: NodesCommands::Drain { node_id } }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::drain_node_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node_id,
                ))?;
            }
            Some(Commands::Nodes { cmd: NodesCommands::Remove { node_id } }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::remove_node_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node_id,
                ))?;
            }
            Some(Commands::Nodes {
                cmd: NodesCommands::Token { cmd: TokenCommands::Create { ttl } },
            }) => {
                // ADR-055 Phase 5a: real enrollment-token issuance. The
                // plaintext is printed exactly once — only its sha256
                // hash is persisted in `enrollment_tokens.json`.
                let ttl = parse_ttl(&ttl)?;
                let mut store = crate::mqtt::enrollment::EnrollmentTokenStore::load(
                    std::path::Path::new(&data_dir),
                );
                let plaintext = store.create_token(ttl);
                println!("Enrollment token created (one-time, TTL {}m):", ttl.as_secs() / 60);
                println!("{plaintext}");
                println!("\nPass it to a node on first boot:");
                println!("  acowork-node start --token <token>");
            }
            None => {
                if self.daemon {
                    tracing::info!("Starting gateway in daemon mode");
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(worker_threads)
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

/// Parse a TTL spec ("30m", "1h", "90s", "2d") into a Duration.
fn parse_ttl(spec: &str) -> Result<std::time::Duration, GatewayError> {
    let spec = spec.trim();
    let (num, unit) = spec.split_at(spec.len().saturating_sub(1));
    let value: u64 = num.parse().map_err(|_| {
        GatewayError::Config(format!(
            "invalid TTL '{spec}' (expected e.g. 30m, 1h, 90s, 2d)"
        ))
    })?;
    let seconds = match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86400,
        _ => {
            return Err(GatewayError::Config(format!(
                "invalid TTL unit '{unit}' (expected s/m/h/d)"
            )))
        }
    };
    Ok(std::time::Duration::from_secs(seconds))
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
    use tracing_subscriber::{reload, layer::SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use acowork_core::logging::ChronoLocalTimer;
    use crate::config::GatewayConfig;

    let env_filter = acowork_core::logging::build_env_filter(level);

    // Log directory: <root>/data/logs/  (sibling of embed.log)
    let log_dir = GatewayConfig::project_data_dir().join("logs");

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "WARN: failed to create log directory {:?}: {}; falling back to stderr-only",
            log_dir, e
        );
        return init_stderr_only(env_filter);
    }

    // Size-based rolling file appender: splits when file exceeds log_file_size_mb
    let max_file_count = if log_file_count > 0 { log_file_count as usize } else { 0 };
    // File appender creation may fail (e.g. macOS sandbox EPERM on $HOME paths,
    // full disk, missing perms). Fall back to stderr-only rather than panicking,
    // so a transient filesystem error does not abort gateway startup.
    let file_appender = match acowork_core::logging::SizeRollingFileAppender::new(
        log_dir.clone(),
        if log_file_size_mb > 0 { log_file_size_mb } else { 10 },
        max_file_count,
    ) {
        Ok(appender) => Arc::new(appender),
        Err(e) => {
            eprintln!(
                "WARN: failed to open log file in {:?}: {}; falling back to stderr-only",
                log_dir, e
            );
            return init_stderr_only(env_filter);
        }
    };
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

/// Initialize a stderr-only tracing subscriber with reload support.
///
/// Used as the fallback when the rolling file appender cannot be opened
/// (sandbox EPERM, missing parent dir, full disk). Keeping the reload
/// handle means the gateway can still push dynamic log-level updates
/// even when the file writer is unavailable.
fn init_stderr_only(env_filter: tracing_subscriber::EnvFilter) -> Option<crate::LogReloadHandle> {
    use tracing_subscriber::{reload, layer::SubscriberExt, util::SubscriberInitExt};
    use acowork_core::logging::ChronoLocalTimer;
    let (filter, handle) = reload::Layer::new(env_filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(CrlfStderr)
                .with_target(false)
                .with_timer(ChronoLocalTimer)
                .compact(),
        )
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
            Some(Commands::Install { package, .. }) => {
                assert_eq!(package, "weather.agent");
            }
            _ => panic!("Expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_start() {
        let cli = Cli::parse_from(["acowork-gateway", "start", "com.example.weather"]);
        match cli.command {
            Some(Commands::Start { agent_id, .. }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Start command"),
        }
    }

    #[test]
    fn test_cli_parse_stop() {
        let cli = Cli::parse_from(["acowork-gateway", "stop", "com.example.weather"]);
        match cli.command {
            Some(Commands::Stop { agent_id, .. }) => {
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
    fn test_cli_parse_nodes_list() {
        let cli = Cli::parse_from(["acowork-gateway", "nodes", "list"]);
        match cli.command {
            Some(Commands::Nodes {
                cmd: NodesCommands::List,
            }) => {}
            _ => panic!("Expected Nodes List command"),
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
            Some(Commands::Upgrade { agent_id, package, .. }) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(package, "weather-v2.agent");
            }
            _ => panic!("Expected Upgrade command"),
        }
    }

    #[test]
    fn test_cli_default_log_level() {
        // Override any env var set in developer's environment so the
        // clap default_value is used.
        unsafe { std::env::set_var("ACOWORK_GATEWAY_LOG_LEVEL", "info") };
        let cli = Cli::parse_from(["acowork-gateway"]);
        assert_eq!(cli.log_level, "info");
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::parse_from(["acowork-gateway", "--log-level", "debug"]);
        assert_eq!(cli.log_level, "debug");
    }

    #[test]
    fn test_cli_parse_token_create() {
        let cli = Cli::parse_from([
            "acowork-gateway",
            "nodes",
            "token",
            "create",
            "--ttl",
            "2h",
        ]);
        match cli.command {
            Some(Commands::Nodes {
                cmd: NodesCommands::Token { cmd: TokenCommands::Create { ttl } },
            }) => assert_eq!(ttl, "2h"),
            _ => panic!("Expected nodes token create command"),
        }
    }

    #[test]
    fn test_parse_ttl_units() {
        assert_eq!(parse_ttl("30m").unwrap(), std::time::Duration::from_secs(1800));
        assert_eq!(parse_ttl("1h").unwrap(), std::time::Duration::from_secs(3600));
        assert_eq!(parse_ttl("90s").unwrap(), std::time::Duration::from_secs(90));
        assert_eq!(parse_ttl("2d").unwrap(), std::time::Duration::from_secs(172800));
        assert!(parse_ttl("30").is_err(), "missing unit must fail");
        assert!(parse_ttl("30x").is_err(), "unknown unit must fail");
        assert!(parse_ttl("abc").is_err(), "non-numeric must fail");
    }
}
