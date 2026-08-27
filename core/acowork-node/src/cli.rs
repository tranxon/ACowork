//! acowork-node CLI (ADR-055 §6.13.2).
//!
//! Phase 2a command surface: `start` / `enroll` / `status`.
//! Phase 2c adds the read-only + emergency-write agent tooling:
//! `agents list/logs/kill`. The remaining command face (`rename`,
//! `leave`, `service install`) lands in Phase 3.
//!
//! Design principle (§6.13.5): the node-local CLI is read-only +
//! node-self-lifecycle tooling — it is NOT a second control plane.
//! install/uninstall/start/stop for agents always go through the
//! Gateway.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{resolve_home, NodeConfig};
use crate::control::NodeControlPlane;
use crate::error::NodeError;
use crate::identity::NodeIdentity;
use crate::state::NodeState;

/// ACowork Node Agent — per-machine Runtime host (ADR-055).
#[derive(Parser)]
#[command(name = "acowork-node")]
#[command(about = "ACowork Node Agent — hosts Agent Runtimes on this machine (ADR-055)")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the node daemon (foreground). Auto-enrolls when
    /// identity.json does not exist yet — one command = deployed.
    Start {
        /// Gateway MQTT broker host.
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_HOST", default_value = "127.0.0.1")]
        gateway_host: String,
        /// Gateway MQTT broker port.
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_PORT", default_value = "19875")]
        gateway_mqtt_port: u16,
        /// Node name (slug). Default: derived from the hostname at
        /// first start; ignored when identity.json already exists.
        #[arg(long, env = "ACOWORK_NODE_NAME")]
        name: Option<String>,
        /// Node data directory (default: $HOME/.acowork/acowork-node).
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Agent package install directory (default: {home}/packages).
        /// Set by the Gateway when spawning the local node to keep the
        /// package inventory on the Gateway's own packages_dir.
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
        /// Enrollment token (Phase 5a — accepted, not yet validated).
        #[arg(long, env = "ACOWORK_NODE_TOKEN")]
        token: Option<String>,
        /// Maximum concurrent Runtime processes (§6.18).
        #[arg(long, env = "ACOWORK_NODE_MAX_AGENTS", default_value = "16")]
        max_agents: u32,
        /// ADR-055 §6.3: the address other machines use to reach this
        /// node's reverse proxy (default 127.0.0.1 = single machine).
        #[arg(long, env = "ACOWORK_NODE_ADVERTISE_HOST", default_value = "127.0.0.1")]
        advertise_host: String,
        /// Reverse-proxy bind address (§6.4, default 0.0.0.0).
        #[arg(long, env = "ACOWORK_NODE_PROXY_BIND", default_value = "0.0.0.0")]
        proxy_bind: String,
        /// Reverse-proxy TCP port (§6.4, default 19900).
        #[arg(long, env = "ACOWORK_NODE_PROXY_PORT", default_value = "19900")]
        proxy_port: u16,
        /// Node-local LSP relay TCP port (ADR-055 §6.7, default 19878).
        #[arg(long, env = "ACOWORK_NODE_LSP_RELAY_PORT", default_value = "19878")]
        lsp_relay_port: u16,
    },
    /// Register the node identity against a Gateway without staying
    /// resident (script / bulk-deployment friendly; idempotent).
    Enroll {
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_HOST", default_value = "127.0.0.1")]
        gateway_host: String,
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_PORT", default_value = "19875")]
        gateway_mqtt_port: u16,
        #[arg(long, env = "ACOWORK_NODE_NAME")]
        name: Option<String>,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_TOKEN")]
        token: Option<String>,
    },
    /// Print this node's identity and daemon state (read-only).
    Status {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Output as JSON (for scripts).
        #[arg(long)]
        json: bool,
    },
    /// Agent tooling (ADR-055 §6.13.2) — read-only + emergency kill.
    Agents {
        #[command(subcommand)]
        cmd: AgentsCommands,
    },
    /// Rename this node (ADR-055 §6.12). The daemon must be stopped.
    Rename {
        /// New node name (slug).
        new_name: String,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_HOST", default_value = "127.0.0.1")]
        gateway_host: String,
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_PORT", default_value = "19875")]
        gateway_mqtt_port: u16,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Decommission this node (ADR-055 §6.13.2): drain local Runtimes,
    /// clear retained state, drop from the Gateway's view.
    Leave {
        /// Skip the graceful drain and go offline immediately.
        #[arg(long)]
        force: bool,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_HOST", default_value = "127.0.0.1")]
        gateway_host: String,
        #[arg(long, env = "ACOWORK_NODE_GATEWAY_PORT", default_value = "19875")]
        gateway_mqtt_port: u16,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// OS service integration (systemd user unit / launchd agent).
    Service {
        #[command(subcommand)]
        cmd: ServiceCommands,
    },
}

/// Node-local agent subcommands (ADR-055 §6.13.2).
#[derive(Subcommand)]
pub enum AgentsCommands {
    /// List locally installed agents (version / running state / PID).
    List {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Tail a Runtime's log file (troubleshooting tool).
    Logs {
        /// Agent ID whose logs to tail.
        agent_id: String,
        /// Follow the log (reserved for Phase 3; currently no-op).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print (default 50).
        #[arg(long, default_value = "50")]
        lines: usize,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Emergency stop (SIGKILL process group). Escape hatch for when
    /// the Gateway is unreachable; state converges via Runtime LWT.
    Kill {
        /// Agent ID to kill.
        agent_id: String,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
}

/// OS service subcommands (ADR-055 §6.13.2).
#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Install the resident-daemon service unit.
    Install {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
    /// Remove the resident-daemon service unit.
    Uninstall {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
}

impl Cli {
    pub fn run(self) -> Result<(), NodeError> {
        match self.command {
            Some(Command::Start {
                gateway_host,
                gateway_mqtt_port,
                name,
                home,
                packages_dir,
                token,
                max_agents,
                advertise_host,
                proxy_bind,
                proxy_port,
                lsp_relay_port,
            }) => {
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    name,
                    token,
                    max_agents,
                    advertise_host,
                    proxy_bind,
                    proxy_port,
                    lsp_relay_port,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::run(config))
            }
            Some(Command::Enroll {
                gateway_host,
                gateway_mqtt_port,
                name,
                home,
                packages_dir,
                token,
            }) => {
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    name,
                    token,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::enroll(config))
            }
            Some(Command::Status { home, json }) => {
                let home = resolve_home(home.as_deref());
                let identity = NodeIdentity::load(&home)?;
                let snapshot = NodeState::load_snapshot(&home);
                if json {
                    let doc = serde_json::json!({
                        "identity": identity,
                        "runtime": snapshot,
                    });
                    println!("{}", serde_json::to_string_pretty(&doc).map_err(|e| {
                        NodeError::Config(format!("serialize status: {e}"))
                    })?);
                } else {
                    print_status(&identity, snapshot.as_ref());
                }
                Ok(())
            }
            Some(Command::Agents { cmd }) => match cmd {
                AgentsCommands::List { home, packages_dir } => {
                    let home = resolve_home(home.as_deref());
                    let packages = packages_dir
                        .unwrap_or_else(|| home.join("packages"));
                    print_agents(&home, &packages);
                    Ok(())
                }
                AgentsCommands::Logs {
                    agent_id,
                    follow,
                    lines,
                    home,
                    packages_dir,
                } => {
                    let home = resolve_home(home.as_deref());
                    let packages = packages_dir
                        .unwrap_or_else(|| home.join("packages"));
                    tail_logs(&agent_id, &packages, lines, follow)
                }
                AgentsCommands::Kill { agent_id, home } => {
                    let home = resolve_home(home.as_deref());
                    kill_agent(&home, &agent_id)
                }
            },
            Some(Command::Rename {
                new_name,
                home,
                gateway_host,
                gateway_mqtt_port,
                packages_dir,
            }) => {
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::rename(config, &new_name))
            }
            Some(Command::Leave {
                force,
                home,
                gateway_host,
                gateway_mqtt_port,
                packages_dir,
            }) => {
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::leave(config, force))
            }
            Some(Command::Service { cmd }) => match cmd {
                ServiceCommands::Install { home } => {
                    let config = NodeConfig {
                        home: resolve_home(home.as_deref()),
                        ..NodeConfig::default()
                    };
                    crate::service::install_service(&config)
                }
                ServiceCommands::Uninstall { home } => {
                    let config = NodeConfig {
                        home: resolve_home(home.as_deref()),
                        ..NodeConfig::default()
                    };
                    crate::service::uninstall_service(&config)
                }
            },
            None => {
                // No subcommand = start (daemon in foreground), per
                // §6.13.2 `acowork-node` with no args.
                let config = NodeConfig::default();
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::run(config))
            }
        }
    }
}

fn print_status(identity: &Option<NodeIdentity>, snapshot: Option<&crate::state::NodeRuntimeSnapshot>) {
    match identity {
        Some(id) => {
            println!("node_id     : {}", id.node_id);
            println!("machine_uid : {}", id.machine_uid);
            println!("enrollment  : {:?}", id.enrollment);
            println!("gateway     : {}", id.gateway_addr.as_deref().unwrap_or("(never connected)"));
            println!("created_at  : {}", id.created_at.to_rfc3339());
            if let Some(at) = id.enrolled_at {
                println!("enrolled_at : {at}");
            }
            if let Some(snap) = snapshot {
                println!("daemon      : {}", if snap.connected { "connected" } else { "disconnected" });
                if let Some(at) = snap.last_connected_at {
                    println!("last_connect: {at}");
                }
                println!("agents      : {}", snap.agent_count);
            } else {
                println!("daemon      : not running (no state.json)");
            }
        }
        None => {
            println!("No node identity found — run `acowork-node start` to create one.");
        }
    }
}

/// Print locally installed agents with version / running state / PID.
///
/// Installed set comes from the packages dir (the node's own inventory
/// authority, §6.5); running state comes from the persisted
/// `state.json` (PID + loopback HTTP port, written by the daemon on
/// start/stop/shutdown).
fn print_agents(home: &Path, packages_dir: &Path) {
    // Rebuild the install table from disk (same logic the daemon uses).
    let mut state = NodeState::new(16);
    crate::package::restore_installed_agents(&mut state, packages_dir);

    let snapshot = NodeState::load_snapshot(home);

    if state.installed_agents.is_empty() {
        println!("No agents installed on this node.");
        return;
    }

    println!("{:<40} {:<12} {:<8} {:<8} HTTP_PORT", "AGENT", "VERSION", "STATE", "PID");
    for info in state.installed_agents.values() {
        let running = snapshot
            .as_ref()
            .and_then(|s| s.agents.iter().find(|a| a.agent_id == info.agent_id));
        let (st, pid, port) = match running {
            Some(a) => ("running", a.pid.to_string(), a.http_port.to_string()),
            None => ("stopped", "-".to_string(), "-".to_string()),
        };
        println!("{:<40} {:<12} {:<8} {:<8} {}", info.agent_id, info.version, st, pid, port);
    }
}

/// Tail a Runtime's most recent log file.
///
/// Logs live under `{install_path}/workspace/logs/` (the Runtime's
/// `--work-dir`), named `YYYYMMDD_HHMMSS.log` by
/// `SizeRollingFileAppender`.
///
/// With `follow` this keeps polling the file for appended lines and
/// switches to the newest file on log rotation (ADR-055 §6.13.2).
fn tail_logs(
    agent_id: &str,
    packages_dir: &Path,
    lines: usize,
    follow: bool,
) -> Result<(), NodeError> {
    use std::io::Write as _;

    let log_dir = packages_dir.join(agent_id).join("workspace").join("logs");

    /// List the `.log` files sorted by name (timestamped, newest last).
    fn latest_log(log_dir: &Path) -> Option<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("log"))
            .collect();
        files.sort();
        files.last().cloned()
    }

    /// Print the trailing `lines` lines of a file, returning its byte
    /// length (used as the follow offset).
    fn print_tail(path: &Path, lines: usize) -> Result<usize, NodeError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            NodeError::Config(format!("Cannot read log '{}': {}", path.display(), e))
        })?;
        let len = content.len();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        for line in &all[start..] {
            println!("{line}");
        }
        Ok(len)
    }

    let Some(mut current) = latest_log(&log_dir) else {
        return Err(NodeError::Config(format!(
            "No log files found in '{}'",
            log_dir.display()
        )));
    };

    let mut last_len = print_tail(&current, lines)?;
    if !follow {
        return Ok(());
    }

    // Follow: poll for appended bytes, switch on rotation.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));

        // Rotation: a newer timestamped file appeared — switch to it.
        if let Some(newest) = latest_log(&log_dir)
            && newest != current
        {
            println!("--- rotated to {} ---", newest.display());
            current = newest;
            last_len = 0;
            continue;
        }

        // Appended bytes. `last_len` is a char boundary (the previous
        // `String` length), so slicing is safe.
        let Ok(content) = std::fs::read_to_string(&current) else {
            continue;
        };
        let len = content.len();
        if len > last_len {
            print!("{}", &content[last_len..]);
            let _ = std::io::stdout().flush();
            last_len = len;
        }
    }
}

/// Emergency-kill a Runtime by PID (from the persisted snapshot).
fn kill_agent(home: &Path, agent_id: &str) -> Result<(), NodeError> {
    let snapshot = NodeState::load_snapshot(home)
        .ok_or_else(|| NodeError::Config("No state.json — is the node daemon running?".to_string()))?;
    let slot = snapshot
        .agents
        .iter()
        .find(|a| a.agent_id == agent_id)
        .ok_or_else(|| NodeError::AgentNotFound(agent_id.to_string()))?;

    // Use the same kill path as the daemon (SIGTERM on Unix via `kill`,
    // taskkill on Windows). SIGKILL semantics are the daemon's reaper
    // concern; here we just terminate the process group.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
    rt.block_on(crate::process::spawn::kill_agent_process(slot.pid))?;
    println!("Sent termination signal to '{}' (PID {})", agent_id, slot.pid);
    Ok(())
}

/// Initialize tracing: stderr + rolling file under `{home}/logs/`.
fn init_tracing(config: &NodeConfig) {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let log_dir = config.home.join("logs");
    let file_layer = std::fs::create_dir_all(&log_dir)
        .ok()
        .and_then(|_| {
            acowork_core::logging::SizeRollingFileAppender::new(log_dir, 10, 10)
                .ok()
                .map(std::sync::Arc::new)
        })
        .map(|appender| {
            tracing_subscriber::fmt::layer()
                .with_writer(appender)
                .with_ansi(false)
                .with_timer(acowork_core::logging::ChronoLocalTimer)
        });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .with_timer(acowork_core::logging::ChronoLocalTimer)
        .compact();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    match file_layer {
        Some(file) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file)
                .init();
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
        }
    }
    acowork_core::logging::install_panic_hook();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_parse_start() {
        let cli = Cli::parse_from([
            "acowork-node", "start",
            "--gateway-host", "192.168.1.10",
            "--gateway-mqtt-port", "19875",
            "--name", "gpu-server",
        ]);
        match cli.command {
            Some(Command::Start { gateway_host, gateway_mqtt_port, name, .. }) => {
                assert_eq!(gateway_host, "192.168.1.10");
                assert_eq!(gateway_mqtt_port, 19875);
                assert_eq!(name.as_deref(), Some("gpu-server"));
            }
            _ => panic!("Expected Start"),
        }
    }

    #[test]
    fn cli_parse_no_subcommand_is_start() {
        let cli = Cli::parse_from(["acowork-node"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_parse_status() {
        let cli = Cli::parse_from(["acowork-node", "status", "--json"]);
        match cli.command {
            Some(Command::Status { json, .. }) => assert!(json),
            _ => panic!("Expected Status"),
        }
    }

    #[test]
    fn cli_parse_enroll_env_and_flags() {
        let cli = Cli::parse_from([
            "acowork-node", "enroll", "--gateway-host", "10.0.0.1", "--token", "tok_x",
        ]);
        match cli.command {
            Some(Command::Enroll { gateway_host, token, .. }) => {
                assert_eq!(gateway_host, "10.0.0.1");
                assert_eq!(token.as_deref(), Some("tok_x"));
            }
            _ => panic!("Expected Enroll"),
        }
    }

    #[test]
    fn cli_parse_agents_list() {
        let cli = Cli::parse_from(["acowork-node", "agents", "list"]);
        match cli.command {
            Some(Command::Agents { cmd: AgentsCommands::List { .. } }) => {}
            _ => panic!("Expected Agents List"),
        }
    }

    #[test]
    fn cli_parse_agents_logs() {
        let cli = Cli::parse_from(["acowork-node", "agents", "logs", "com.example", "--lines", "20"]);
        match cli.command {
            Some(Command::Agents {
                cmd: AgentsCommands::Logs { agent_id, lines, .. },
            }) => {
                assert_eq!(agent_id, "com.example");
                assert_eq!(lines, 20);
            }
            _ => panic!("Expected Agents Logs"),
        }
    }

    #[test]
    fn cli_parse_agents_kill() {
        let cli = Cli::parse_from(["acowork-node", "agents", "kill", "com.example"]);
        match cli.command {
            Some(Command::Agents {
                cmd: AgentsCommands::Kill { agent_id, .. },
            }) => assert_eq!(agent_id, "com.example"),
            _ => panic!("Expected Agents Kill"),
        }
    }

    #[test]
    fn cli_parse_rename() {
        let cli = Cli::parse_from(["acowork-node", "rename", "gpu-2"]);
        match cli.command {
            Some(Command::Rename { new_name, .. }) => assert_eq!(new_name, "gpu-2"),
            _ => panic!("Expected Rename"),
        }
    }

    #[test]
    fn cli_parse_leave_force() {
        let cli = Cli::parse_from(["acowork-node", "leave", "--force"]);
        match cli.command {
            Some(Command::Leave { force, .. }) => assert!(force),
            _ => panic!("Expected Leave"),
        }
    }

    #[test]
    fn cli_parse_service_install() {
        let cli = Cli::parse_from(["acowork-node", "service", "install"]);
        match cli.command {
            Some(Command::Service {
                cmd: ServiceCommands::Install { .. },
            }) => {}
            _ => panic!("Expected Service Install"),
        }
    }
}
