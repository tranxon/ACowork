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
    pub gateway_endpoint: Option<String>,

    /// Gateway Unix socket path for IPC connection.
    /// When omitted, the runtime runs in standalone mode without Gateway.
    #[arg(long, env = "ROLLBALL_GATEWAY_SOCKET")]
    pub gateway_socket: Option<String>,

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
            .map_err(crate::error::RuntimeError::Io)?;

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

/// Attempt to connect to Gateway via the given socket path.
/// Returns Some(client) on success, None on failure (graceful fallback to standalone mode).
async fn connect_gateway_client(socket_path: &str) -> Option<crate::ipc::client::GatewayClient> {
    match crate::ipc::client::GatewayClient::connect(socket_path) {
        Ok(client) => {
            if let Err(e) = client.connect_transport(socket_path).await {
                tracing::warn!(
                    "Failed to connect transport to Gateway at {}: {}",
                    socket_path,
                    e
                );
                return None;
            }
            tracing::info!("Connected to Gateway at {}", socket_path);
            Some(client)
        }
        Err(e) => {
            tracing::warn!("Failed to create Gateway client for {}: {}", socket_path, e);
            None
        }
    }
}

/// Async entry point after tokio runtime is initialized
async fn async_main(config: RuntimeConfig) -> Result<()> {
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt;
    use crate::agent::context::ContextBuilder;
    use crate::agent::loop_::AgentLoop;
    use crate::tools::builtin;
    use crate::tools::registry::ToolRegistry;
    use crate::providers::router::create_provider;

    // Step 0: Connect to Gateway if socket path is provided
    let ipc_client = if let Some(socket_path) = config.get_gateway_address() {
        connect_gateway_client(socket_path).await
    } else {
        None
    };
    if ipc_client.is_some() {
        tracing::info!("Gateway IPC client initialized");
    } else {
        tracing::info!("Running in standalone mode (no Gateway)");
    }

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

    // Step 3: Initialize LLM Provider
    let api_key = resolve_api_key(&loaded.manifest);
    let base_url = std::env::var("ROLLBALL_LLM_BASE_URL").ok();
    let provider = create_provider(
        &loaded.manifest.llm.provider,
        api_key.as_deref(),
        base_url.as_deref(),
    );
    tracing::info!(
        provider = %provider.name(),
        model = %loaded.manifest.llm.model,
        "Provider initialized"
    );

    // Step 4: Build tool registry + activate by manifest
    let mut registry = ToolRegistry::new();
    for tool in builtin::all_builtin_tools(&config.work_dir, &config.agent_id) {
        registry.register(tool);
    }
    let active_tools = registry.activate(&loaded.manifest, &config.work_dir, 60);
    tracing::info!(
        total = registry.all().len(),
        active = active_tools.len(),
        "Tools activated"
    );

    // Step 5: Build tool definitions for LLM context
    let tool_specs: Vec<(String, serde_json::Value)> = active_tools
        .iter()
        .map(|t| {
            let spec = t.spec();
            (spec.name.clone(), serde_json::to_value(&spec).unwrap_or_default())
        })
        .collect();
    let tool_definitions = crate::agent::context::build_tool_definitions(
        &loaded.manifest,
        &tool_specs,
    );

    // Step 6: Build context builder
    let context_builder = ContextBuilder::new(system_prompt)
        .with_tools(tool_definitions);

    // Step 7: Create budget (unlimited for standalone mode)
    let budget = rollball_core::Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    // Step 8: Create AgentLoop
    let (mut agent_loop, _inbound_tx) = AgentLoop::new(
        config.clone(),
        loaded.manifest.clone(),
        provider,
        active_tools,
        budget,
        ipc_client,
    );

    // Step 9: Run interactive chat loop
    run_chat_loop(&mut agent_loop, &context_builder).await
}

/// Resolve API key from environment variables (standalone mode)
///
/// Priority:
/// 1. ROLLBALL_LLM_API_KEY (generic override)
/// 2. OPENAI_API_KEY / OLLAMA_API_KEY (provider-specific)
fn resolve_api_key(manifest: &rollball_core::AgentManifest) -> Option<String> {
    if let Ok(key) = std::env::var("ROLLBALL_LLM_API_KEY")
        && !key.is_empty() {
        return Some(key);
    }

    let env_key = match manifest.llm.provider.as_str() {
        "ollama" => "OLLAMA_API_KEY",
        _ => "OPENAI_API_KEY",
    };

    std::env::var(env_key).ok().filter(|k| !k.is_empty())
}

/// Run interactive stdin chat loop
async fn run_chat_loop(
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    context_builder: &crate::agent::context::ContextBuilder,
) -> Result<()> {
    use std::io::{self, BufRead, Write};

    println!("RollBall Agent Runtime — type messages and press Enter (Ctrl+C to exit)");
    println!();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line.map_err(crate::error::RuntimeError::Io)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == "/quit" || trimmed == "/exit" {
            println!("Goodbye!");
            return Ok(());
        }

        match agent_loop.run(trimmed, context_builder).await {
            Ok(response) => {
                println!("
--- Agent ---
{response}
");
            }
            Err(e) => {
                tracing::error!(error = %e, "Agent loop error");
                println!("
--- Error ---
{e}
");
            }
        }

        stdout.flush().ok();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_gateway_socket_arg() {
        let cli = Cli::parse_from([
            "rollball-runtime",
            "--agent-id",
            "com.test.agent",
            "--package-path",
            "/tmp/test.agent",
            "--work-dir",
            "/tmp/work",
            "--gateway-socket",
            "unix:///tmp/gateway.sock",
        ]);
        assert_eq!(cli.agent_id, "com.test.agent");
        assert_eq!(cli.package_path, "/tmp/test.agent");
        assert_eq!(cli.work_dir, "/tmp/work");
        assert_eq!(cli.gateway_socket, Some("unix:///tmp/gateway.sock".to_string()));
    }

    #[tokio::test]
    async fn test_gateway_client_connection_failure_graceful() {
        // Use a non-existent socket path to force connection failure
        let client = connect_gateway_client("unix:///nonexistent/socket/path.sock").await;
        assert!(
            client.is_none(),
            "Should gracefully fallback to None on connection failure"
        );
    }
}
