//! CLI definitions for Agent Runtime

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::RuntimeConfig;
use crate::error::Result;

/// Agent Runtime CLI
#[derive(Parser)]
#[command(name = "rollball-runtime")]
#[command(about = "Agent Runtime - unified execution engine for .agent packages")]
#[command(version)]
pub struct Cli {
    /// Agent ID (reverse-domain identifier, e.g., com.example.weather)
    #[arg(long, env = "ROLLBALL_AGENT_ID")]
    pub agent_id: String,

    /// Path to .agent package (ZIP file or extracted directory)
    #[arg(long, env = "ROLLBALL_PACKAGE_PATH")]
    pub package_path: String,

    /// Working directory for the agent
    #[arg(long, env = "ROLLBALL_WORK_DIR")]
    pub work_dir: String,

    /// Gateway endpoint (e.g., unix:///tmp/agent-gateway.sock)
    #[arg(long, env = "ROLLBALL_GATEWAY_ENDPOINT")]
    pub gateway_endpoint: String,

    /// Enable developer mode (debug protocol)
    #[arg(long, default_value = "false")]
    pub dev_mode: bool,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", env = "ROLLBALL_LOG_LEVEL")]
    pub log_level: String,

    /// Path to manifest.toml (overrides package-embedded manifest)
    #[arg(long)]
    pub manifest_path: Option<String>,

    /// Config directory for the agent
    #[arg(long, env = "ROLLBALL_CONFIG_DIR")]
    pub config_dir: Option<String>,
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<()> {
        // Initialize tracing/logging
        self.init_tracing();

        // Build runtime config from CLI args
        let config = RuntimeConfig::from_cli(&self);

        tracing::info!(
            agent_id = %config.agent_id,
            package_path = %config.package_path,
            work_dir = %config.work_dir,
            "Starting Agent Runtime"
        );

        // Create tokio runtime and run async main
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| crate::error::RuntimeError::Io(e))?;

        rt.block_on(async_main(config))
    }

    /// Initialize tracing subscriber
    fn init_tracing(&self) {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new(&self.log_level)),
            )
            .with_target(false)
            .with_thread_ids(false)
            .with_file(false)
            .init();
    }
}

/// Async entry point after tokio runtime is initialized
async fn async_main(config: RuntimeConfig) -> Result<()> {
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt;

    // Step 1: Load .agent package
    tracing::info!(path = %config.package_path, "Loading .agent package");
    let loaded = load_package(std::path::Path::new(&config.package_path))?;
    tracing::info!(
        agent_id = %loaded.manifest.agent_id,
        name = %loaded.manifest.name,
        "Package loaded successfully"
    );

    // Step 2: Build system prompt
    let system_prompt = build_system_prompt(&loaded.package_dir)?;
    tracing::debug!(
        prompt_len = system_prompt.len(),
        "System prompt built"
    );

    // TODO: Step 3-9 — initialize provider, tool registry, history, IPC, and run main loop
    tracing::warn!("Agent main loop not yet implemented — exiting after package load");

    Ok(())
}
