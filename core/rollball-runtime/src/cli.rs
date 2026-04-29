//! CLI definitions for Agent Runtime

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::RuntimeConfig;
use crate::error::Result;

/// Retry interval when Gateway recv encounters a transient error
const GATEWAY_RECV_RETRY_INTERVAL_MS: u64 = 100;

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
async fn connect_gateway_client(socket_path: &str, agent_id: &str, version: &str) -> Option<crate::ipc::client::GatewayClient> {
    let mut client = crate::ipc::client::GatewayClient::new(socket_path);
    match client.connect_and_register(agent_id, version).await {
        Ok(()) => {
            tracing::info!("Connected and registered with Gateway at {}", socket_path);
            Some(client)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to connect to Gateway at {}: {}",
                socket_path,
                e
            );
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

    // Step 1: Load .agent package (before Gateway connection so we know agent_id)
    tracing::info!(path = %config.package_path, "Loading .agent package");
    let loaded = load_package(std::path::Path::new(&config.package_path))?;
    tracing::info!(
        agent_id = %loaded.manifest.agent_id,
        name = %loaded.manifest.name,
        "Package loaded successfully"
    );

    // Step 2: Connect to Gateway if socket path is provided
    let mut ipc_client = if let Some(socket_path) = config.get_gateway_address() {
        connect_gateway_client(socket_path, &loaded.manifest.agent_id, &loaded.manifest.version).await
    } else {
        None
    };
    if ipc_client.is_some() {
        tracing::info!("Gateway IPC client initialized");
    } else {
        tracing::info!("Running in standalone mode (no Gateway)");
    }

    // Step 3: Build system prompt
    let system_prompt = build_system_prompt(&loaded.package_dir)?;
    tracing::debug!(
        prompt_len = system_prompt.len(),
        "System prompt built"
    );

    // Step 3: Initialize LLM Provider
    //
    // In Gateway mode: LLMConfigDelivery is MANDATORY.
    //   The user's provider/key/model MUST come from Gateway IPC.
    //   Manifest's suggested_provider/model is for REFERENCE only — never used at runtime.
    //   This satisfies PRD GTW-05 and SEC-07 (no env-var key distribution).
    //
    // In Standalone mode: use manifest suggested_provider + env vars (development only).
    let (provider, resolved_model) = if let Some(ref mut client) = ipc_client {
        // Gateway mode: LLMConfigDelivery is required
        match client.recv_llm_config().await {
            Ok(llm_config) => {
                tracing::info!(
                    provider = %llm_config.provider,
                    model = ?llm_config.model,
                    source = "Gateway IPC",
                    "LLM config received from Gateway"
                );
                let p = crate::providers::router::create_provider(
                    &llm_config.provider,
                    llm_config.api_key.as_deref(),
                    llm_config.base_url.as_deref(),
                );
                // Model is required — if Gateway didn't specify one, use the first
                // example model from the provider definition as a sensible default.
                let resolved = llm_config.model.unwrap_or_else(|| {
                    let fallback = crate::providers::router::default_model_for_provider(
                        &llm_config.provider,
                    );
                    tracing::warn!(
                        provider = %llm_config.provider,
                        fallback_model = %fallback,
                        "No model specified by Gateway, using provider default"
                    );
                    fallback
                });
                (p, resolved)
            }
            Err(e) => {
                // CRITICAL: In Gateway mode, no LLM config means the agent cannot function.
                tracing::error!(
                    error = %e,
                    "CRITICAL: Failed to receive LLM config from Gateway. \
                     Agent cannot process messages until API key is configured."
                );
                // Return a provider that returns clear error messages on any request
                let p = crate::providers::router::create_noop_provider();
                (p, "no-model".to_string())
            }
        }
    } else {
        // Standalone mode: use manifest suggested_provider + env vars
        let api_key = resolve_api_key(&loaded.manifest);
        let base_url = std::env::var("ROLLBALL_LLM_BASE_URL").ok();
        let p = build_runtime_provider(&loaded.manifest, api_key.as_deref(), base_url.as_deref());
        tracing::info!(
            provider = %p.name(),
            model = %loaded.manifest.llm.suggested_model,
            source = "manifest + env",
            "Provider initialized (standalone mode)"
        );
        (p, loaded.manifest.llm.suggested_model.clone())
    };

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

    // Step 6: Build context builder (with identity injection from Gateway)
    let identity_context = load_identity_delivery(&config.work_dir);
    let mut context_builder = ContextBuilder::new(system_prompt)
        .with_identity(identity_context)
        .with_tools(tool_definitions);

    // If Gateway delivered a model override, apply it so that Gateway's default_model
    // takes precedence over the manifest's suggested_model.
    // In standalone mode, resolved_model equals manifest.llm.suggested_model (no override needed).
    if resolved_model != loaded.manifest.llm.suggested_model {
        tracing::info!(
            model = %resolved_model,
            manifest_model = %loaded.manifest.llm.suggested_model,
            "Applying Gateway model override"
        );
        context_builder = context_builder.with_override_model(resolved_model);
    }

    // Step 7: Create budget (unlimited for standalone mode)
    let budget = rollball_core::Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    // Step 8: Create AgentLoop with optional streaming chunk channel
    // In Gateway mode, each StreamEvent::Content delta is forwarded through
    // the on_chunk mpsc channel, then relayed to Gateway via TYPE_STREAM_CHUNK.
    let (chunk_tx, chunk_rx) = if ipc_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::ChunkEvent>(256);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let (mut agent_loop, inbound_tx) = AgentLoop::new(
        config.clone(),
        loaded.manifest.clone(),
        provider,
        active_tools,
        budget,
        chunk_tx,
    );

    // Step 9: Run the appropriate loop based on connection mode
    if let Some(mut client) = ipc_client {
        // Gateway mode: run message loop to receive messages from Gateway
        tracing::info!("Running in Gateway mode");

        // Spawn chunk relay task: consumes ChunkEvent from mpsc channel and
        // forwards each delta to Gateway via TYPE_STREAM_CHUNK (no response wait).
        // This uses a second IPC connection dedicated to outbound streaming chunks,
        // so it doesn't interfere with the main connection's recv/send cycle.
        let chunk_relay = if let Some(mut chunk_rx) = chunk_rx {
            let agent_id = config.agent_id.clone();
            let version = loaded.manifest.version.clone();
            let socket_path = config.get_gateway_address()
                .expect("gateway address must be set in Gateway mode")
                .to_string();
            Some(tokio::spawn(async move {
                // Connect a second IPC client for chunk relay
                let mut chunk_client = crate::ipc::client::GatewayClient::new(&socket_path);
                if let Err(e) = chunk_client.connect_and_register_with_role(&agent_id, &version, "chunk-relay").await {
                    tracing::error!("Chunk relay IPC connection failed: {}", e);
                    return;
                }
                tracing::info!("Chunk relay connected to Gateway");

                while let Some(event) = chunk_rx.recv().await {
                    match event {
                        crate::agent::loop_::ChunkEvent::Delta(delta) => {
                            let params = serde_json::json!({
                                "content": delta,
                            });
                            if let Err(e) = chunk_client
                                .send_stream_chunk("http-ws", "agent_chunk", params, true)
                                .await
                            {
                                tracing::debug!("Chunk relay send failed: {}", e);
                            }
                        }
                    }
                }
                tracing::debug!("Chunk relay task ended");
            }))
        } else {
            None
        };

        let result = run_gateway_loop(agent_loop, inbound_tx, &mut client, context_builder).await;

        // Chunk relay task will end when chunk_rx is dropped (agent_loop dropped)
        if let Some(handle) = chunk_relay {
            let _ = handle.await;
        }

        result
    } else {
        // Standalone mode: run interactive stdin chat loop
        tracing::info!("Running in standalone mode");
        run_chat_loop(&mut agent_loop, &context_builder).await
    }
}

/// Load identity delivery from the Gateway-injected `.identity_delivery.json`
/// in the agent workspace.
///
/// When Gateway spawns an Agent, it writes identity entries to this file
/// based on the agent's `identity_deps` manifest declaration.
/// The Runtime reads this file during cold start and formats it for
/// System Prompt injection.
fn load_identity_delivery(work_dir: &str) -> Option<String> {
    let identity_path = std::path::Path::new(work_dir).join(".identity_delivery.json");
    if !identity_path.exists() {
        return None;
    }

    match std::fs::read_to_string(&identity_path) {
        Ok(content) => {
            match serde_json::from_str::<Vec<rollball_core::identity::IdentityEntry>>(&content) {
                Ok(entries) => {
                    if entries.is_empty() {
                        return None;
                    }
                    // Format identity entries as readable text for System Prompt
                    let mut formatted = String::from("User identity information:\n");
                    for entry in &entries {
                        if !entry.value.is_empty() {
                            formatted.push_str(&format!(
                                "- {}: {} (confidence: {}%%)\n",
                                entry.field, entry.value, (entry.confidence * 100.0) as u32
                            ));
                        } else {
                            formatted.push_str(&format!(
                                "- {}: (not yet provided)\n",
                                entry.field
                            ));
                        }
                    }
                    tracing::info!(
                        entries = entries.len(),
                        "Identity delivery loaded from workspace"
                    );
                    Some(formatted)
                }
                Err(e) => {
                    tracing::warn!("Failed to parse identity delivery: {}", e);
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read identity delivery: {}", e);
            None
        }
    }
}

/// Build the runtime provider with multi-provider routing support.
///
/// When the manifest declares `providers` + `routing`, constructs a
/// ProviderRegistry and builds a ReliableProvider with fallback chain.
/// Otherwise falls back to a simple single provider.
fn build_runtime_provider(
    manifest: &rollball_core::AgentManifest,
    default_api_key: Option<&str>,
    default_base_url: Option<&str>,
) -> std::sync::Arc<dyn rollball_core::providers::traits::Provider> {
    use crate::providers::registry::{ProviderRegistry, RoutingStrategy};
    use crate::providers::router::create_provider;

    // If no multi-provider config, use simple single provider
    if manifest.llm.providers.is_empty() {
        return create_provider(
            &manifest.llm.suggested_provider,
            default_api_key,
            default_base_url,
        );
    }

    // Build ProviderRegistry from manifest
    let strategy = manifest.llm.routing
        .as_ref()
        .map(|r| RoutingStrategy::from_str(&r.strategy))
        .unwrap_or(RoutingStrategy::QualityPriority);

    let registry = ProviderRegistry::with_strategy(strategy);

    // Register each provider from manifest
    for (name, config) in &manifest.llm.providers {
        let api_key = config.api_key_ref.as_deref()
            .or(default_api_key);
        let base_url = config.base_url.as_deref()
            .or(default_base_url);
        let provider = create_provider(name, api_key, base_url);
        let models = vec![config.model.clone()];
        registry.register_provider(name, provider, models);
    }

    // Also register the primary provider if not already in providers map
    if !manifest.llm.providers.contains_key(&manifest.llm.suggested_provider) {
        let primary = create_provider(
            &manifest.llm.suggested_provider,
            default_api_key,
            default_base_url,
        );
        registry.register_provider(
            &manifest.llm.suggested_provider,
            primary,
            vec![manifest.llm.suggested_model.clone()],
        );
    }

    // Build ReliableProvider with fallback chain
    match registry.build_reliable_provider(&manifest.llm.suggested_provider, &manifest.llm.suggested_model) {
        Some(reliable) => {
            tracing::info!(
                primary = %manifest.llm.suggested_provider,
                model = %manifest.llm.suggested_model,
                strategy = %strategy,
                "Built ReliableProvider with fallback chain"
            );
            std::sync::Arc::new(reliable)
        }
        None => {
            tracing::warn!("Failed to build ReliableProvider, falling back to single provider");
            create_provider(
                &manifest.llm.suggested_provider,
                default_api_key,
                default_base_url,
            )
        }
    }
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

    let env_key = match manifest.llm.suggested_provider.as_str() {
        "ollama" => "OLLAMA_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "anthropic" | "claude" => "ANTHROPIC_API_KEY",
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

/// Run Gateway message loop — receives messages from Gateway and processes them.
///
/// This loop:
/// 1. Receives IntentReceived messages from Gateway via IPC
/// 2. Checks remaining budget before processing each message
/// 3. Runs the agent loop for each message
/// 4. Sends responses back to Gateway
async fn run_gateway_loop(
    mut agent_loop: crate::agent::loop_::AgentLoop,
    _inbound_tx: tokio::sync::mpsc::Sender<crate::agent::inbound::InboundMessage>,
    ipc_client: &mut crate::ipc::client::GatewayClient,
    context_builder: crate::agent::context::ContextBuilder,
) -> Result<()> {
    use rollball_core::protocol::GatewayResponse;

    // Retrieve the provider name for budget queries
    let budget_provider = agent_loop.manifest().llm.suggested_provider.clone();

    tracing::info!("Gateway message loop started");

    // Main message loop — receive messages from Gateway and process them
    loop {
        match ipc_client.recv_message().await {
            Ok(Some(response)) => {
                tracing::debug!("Received Gateway message: {:?}", response);

                match response {
                    GatewayResponse::IntentReceived { from, action, params } => {
                        tracing::info!("Received intent from {}: {}", from, action);

                        // Budget pre-check: skip processing if budget is exhausted
                        if let Ok((remaining_tokens, _)) = ipc_client.query_budget(&budget_provider).await {
                            if remaining_tokens == 0 {
                                tracing::warn!(
                                    "Budget exhausted for provider={}, skipping message from {}",
                                    budget_provider, from
                                );
                                let error_params = serde_json::json!({
                                    "content": "Budget exhausted — cannot process this message",
                                    "message_id": params.get("message_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("unknown"),
                                });
                                let _ = ipc_client.send_intent(&from, "agent_error", error_params, false).await;
                                continue;
                            }
                        }
                        // If budget query fails (e.g. provider not tracked), proceed anyway

                        // Extract message content from params
                        let content = params.get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let message_id = params.get("message_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("msg-{}", chrono::Utc::now().timestamp_millis()));

                        // Process the message through the agent loop
                        match agent_loop.run(&content, &context_builder).await {
                            Ok(response_text) => {
                                tracing::info!("Agent response: {}", &response_text[..response_text.len().min(100)]);

                                // Send response back to Gateway via bridge channel
                                // Target is the HTTP client that originated the message
                                let reply_target = &from;
                                let intent_params = serde_json::json!({
                                    "content": response_text,
                                    "message_id": message_id,
                                });

                                match ipc_client.send_intent(reply_target, "agent_response", intent_params, false).await {
                                    Ok(_) => tracing::debug!("Response sent to {}", reply_target),
                                    Err(e) => tracing::error!("Failed to send response: {}", e),
                                }
                            }
                            Err(e) => {
                                tracing::error!("Agent error: {}", e);

                                // Send error response
                                let error_params = serde_json::json!({
                                    "content": format!("Error: {}", e),
                                    "message_id": message_id,
                                });
                                let _ = ipc_client.send_intent(&from, "agent_error", error_params, false).await;
                            }
                        }
                    }
                    // Ignore other push messages (CapabilityUpdate, etc.)
                    GatewayResponse::LLMConfigDelivery { provider, model, api_key, base_url, models: _ } => {
                        tracing::info!(
                            provider = %provider,
                            model = ?model,
                            "Received LLMConfigDelivery at runtime — updating provider"
                        );
                        let new_provider = crate::providers::router::create_provider(
                            &provider,
                            Some(&api_key),
                            base_url.as_deref(),
                        );
                        let resolved = model.unwrap_or_else(|| {
                            let fallback = crate::providers::router::default_model_for_provider(&provider);
                            tracing::warn!(
                                provider = %provider,
                                fallback_model = %fallback,
                                "No model in runtime LLMConfigDelivery, using provider default"
                            );
                            fallback
                        });
                        agent_loop.update_provider(new_provider, resolved);
                    }
                    _ => {
                        tracing::debug!("Ignoring non-IntentReceived Gateway message");
                    }
                }
            }
            Ok(None) => {
                tracing::info!("Gateway connection closed");
                break;
            }
            Err(e) => {
                tracing::error!("Gateway recv error: {}", e);
                // Don't break on transient errors — try to continue
                tokio::time::sleep(std::time::Duration::from_millis(GATEWAY_RECV_RETRY_INTERVAL_MS)).await;
            }
        }
    }

    tracing::info!("Gateway message loop ended");
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
        let client = connect_gateway_client("unix:///nonexistent/socket/path.sock", "com.test", "1.0.0").await;
        assert!(
            client.is_none(),
            "Should gracefully fallback to None on connection failure"
        );
    }
}
