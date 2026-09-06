//! CLI argument parsing for the standalone acowork-doc process (ADR-064).

use std::path::PathBuf;

use clap::Parser;

/// CLI arguments for the acowork-doc standalone process.
#[derive(Parser)]
#[command(
    name = "acowork-doc",
    about = "ACowork online document library service (standalone process)"
)]
pub struct Cli {
    /// HTTP listen address.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP listen port (default 18081; auto-increments on conflict, max +20).
    #[arg(long)]
    pub port: Option<u16>,

    /// Data directory override (default `$HOME/.acowork/acowork-doc`).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Write the actual bound port to this file (read by the Gateway supervisor).
    #[arg(long)]
    pub port_file: Option<PathBuf>,

    /// Gateway health URL (ADR-018): self-exit when the Gateway is unreachable.
    #[arg(long)]
    pub gateway_health_url: Option<String>,

    /// Gateway health probe interval (ms, default 10000 = 10s).
    #[arg(long, default_value = "10000")]
    pub gateway_health_interval_ms: u64,

    /// Gateway unreachable self-exit timeout (ms, default 300000 = 5min).
    #[arg(long, default_value = "300000")]
    pub gateway_health_timeout_ms: u64,

    /// Override for update-request TTL (hours) forwarded by the Gateway.
    #[arg(long)]
    pub request_ttl_hours: Option<u32>,

    /// Log level.
    #[arg(long, default_value = "info")]
    pub log_level: String,
}
