//! CLI definitions for Agent Runtime
// ADR-034 Phase 8 cleanup: gRPC-era legacy references in cli.rs are preserved
// for CLI arg backward compatibility. The gRPC gateway was superseded by MQTT
// transport (Phase 1-5). These aliases are kept as dead code for reference.
use crate::agent::inbound::InboundMessage;
use crate::agent::session::{SessionManager, SessionMessage};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent_config::AgentMcpConfig;
use crate::config::RuntimeConfig;
use crate::error::Result;
use acowork_core::protocol::{McpListItem, ProviderListItem};
use clap::Parser;
use std::sync::Arc;

use acowork_core::logging::ChronoLocalTimer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, reload, util::SubscriberInitExt};

/// Type alias for the reload handle used to dynamically change log level.
pub type LogReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Global reference to the SizeRollingFileAppender for runtime log rotation.
/// Set by init_tracing() and read by the LogRotate gRPC handler.
static FILE_APPENDER: std::sync::OnceLock<Arc<acowork_core::logging::SizeRollingFileAppender>> =
    std::sync::OnceLock::new();

/// Agent Runtime CLI
#[derive(Parser)]
#[command(name = "acowork-runtime")]
#[command(about = "Agent Runtime - unified execution engine for .agent packages")]
#[command(version)]
pub struct Cli {
    /// Agent ID (reverse-domain identifier, e.g., com.example.weather)
    #[arg(long, env = "ACOWORK_AGENT_ID")]
    pub agent_id: String,

    /// Path to .agent package (ZIP file or extracted directory)
    #[arg(long, env = "ACOWORK_PACKAGE_PATH")]
    pub package_path: String,

    /// Working directory for the agent
    #[arg(long, env = "ACOWORK_WORK_DIR")]
    pub work_dir: String,

    /// Gateway gRPC endpoint URL (e.g., http://127.0.0.1:19877)
    #[arg(long, env = "ACOWORK_GATEWAY_ENDPOINT")]
    pub gateway_endpoint: Option<String>,

    /// Gateway endpoint URL (deprecated, use --gateway-endpoint).
    #[arg(long, env = "ACOWORK_GATEWAY_SOCKET", hide = true)]
    pub gateway_socket: Option<String>,

    /// Enable developer mode (debug protocol)
    #[arg(long, default_value = "false")]
    pub dev_mode: bool,

    /// Debug WebSocket server port (used with --dev-mode).
    /// Gateway assigns a unique port per agent to avoid conflicts.

    /// Defaults to 19878 when not specified.
    #[arg(long, default_value = "19878")]
    pub debug_port: u16,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", env = "ACOWORK_LOG_LEVEL")]
    pub log_level: String,

    /// Log file maximum size in MB before auto-split (0 = no split, default 10)
    #[arg(long, default_value = "10", env = "ACOWORK_LOG_FILE_SIZE_MB")]
    pub log_file_size_mb: u64,

    /// Maximum number of log files to keep (0 = unlimited, default 20)
    #[arg(long, default_value = "20", env = "ACOWORK_LOG_FILE_COUNT")]
    pub log_file_count: u64,

    /// Path to manifest.toml (overrides package-embedded manifest)
    #[arg(long)]
    pub manifest_path: Option<String>,

    /// Config directory for the agent
    #[arg(long, env = "ACOWORK_CONFIG_DIR")]
    pub config_dir: Option<String>,

    /// ADR-033: MQTT broker port for Runtime MQTT client.
    /// When set, Runtime connects to the Gateway's embedded MQTT broker.
    #[arg(long, env = "ACOWORK_MQTT_PORT")]
    pub mqtt_port: Option<u16>,

    /// ADR-033: Runtime localhost HTTP server port.
    /// When set, Runtime starts a local HTTP endpoint for Desktop discovery.
    #[arg(long, env = "ACOWORK_HTTP_PORT")]
    pub http_port: Option<u16>,
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<()> {
        // Print version info
        let version = env!("CARGO_PKG_VERSION");
        println!("ACowork Runtime v{version}");

        // Initialize tracing/logging and obtain reload handle
        let reload_handle = self.init_tracing();

        // Install global panic hook AFTER tracing is initialized so panic
        // messages are captured in both stderr and the rolling log file.
        acowork_core::logging::install_panic_hook();

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
        rt.block_on(async_main(config, reload_handle))
    }

    /// Initialize tracing subscriber with both stderr and file output.
    ///
    /// Logs are written to stderr (for Gateway capture) AND to
    /// `{work_dir}/logs/YYYYMMDD_HHMMSS.log` for user inspection.
    ///
    /// Returns a reload handle that allows dynamic log level changes
    /// at runtime (e.g. when Gateway pushes LogLevelUpdate).
    fn init_tracing(&self) -> Option<LogReloadHandle> {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&self.log_level));

        // Ensure the log directory exists under work_dir
        let log_dir = std::path::Path::new(&self.work_dir).join("logs");
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            // Fall back to stderr-only if we cannot create the log directory
            eprintln!(
                "WARN: failed to create log directory {:?}: {}; falling back to stderr-only",
                log_dir, e
            );

            // Fallback: use reload::Layer even for stderr-only so we can

            // still dynamically adjust log level at runtime.
            let (filter, reload_handle) = reload::Layer::new(env_filter);
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(false)
                        .with_thread_ids(false)
                        .with_file(false)
                        .with_timer(ChronoLocalTimer)
                        .compact(),
                )
                .init();
            return Some(reload_handle);
        }

        let max_mb = if self.log_file_size_mb > 0 {
            self.log_file_size_mb
        } else {
            10
        };
        let max_file_count = if self.log_file_count > 0 {
            self.log_file_count as usize
        } else {
            0
        };
        let file_appender = Arc::new(acowork_core::logging::SizeRollingFileAppender::new(
            log_dir,
            max_mb,
            max_file_count,
        ));

        // Store for LogRotate gRPC handler
        let _ = FILE_APPENDER.set(file_appender.clone());
        let (filter, reload_handle) = reload::Layer::new(env_filter);
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .with_file(false)
            .with_ansi(cfg!(not(windows))) // Enable ANSI on non-Windows, disable on Windows
            .with_timer(ChronoLocalTimer)
            .compact();
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
        Some(reload_handle)
    }
}

// ── Resource cache (version-driven diff sync) ─────────────────────────

/// Runtime-side resource cache stored in workspace/config/resource_cache.json.
/// Stores versions (for diff sync) and optionally cached provider/MCP lists
/// (for use when Gateway reports "same version, no update needed").
/// API keys are NEVER stored in this file — they come from the live provider_key_vault.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RuntimeResourceCache {
    #[serde(default)]
    pub(crate) provider_list_version: u64,
    #[serde(default)]
    pub(crate) mcp_list_version: u64,
    #[serde(default)]
    pub(crate) search_list_version: u64,
    #[serde(default)]
    pub(crate) user_profile_version: u64,
    /// Cached provider list (without api keys — keys come from vault).
    /// None when no cache exists yet (first start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) providers: Option<Vec<ProviderListItem>>,
    /// Cached MCP server list (without auth tokens — tokens come from vault).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mcps: Option<Vec<McpListItem>>,
}

/// Resource cache file path in agent workspace config directory.
pub(crate) fn resource_cache_path(work_dir: &std::path::Path) -> std::path::PathBuf {
    work_dir.join("config").join("resource_cache.json")
}

/// Read the full runtime resource cache (versions + cached lists).
/// Returns default (versions=0, no lists) if file is missing or corrupt.
pub(crate) fn read_resource_cache(work_dir: &std::path::Path) -> RuntimeResourceCache {
    let path = resource_cache_path(work_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<RuntimeResourceCache>(&raw).unwrap_or_else(|e| {
            tracing::warn!(path=%path.display(), error=%e, "Failed to parse resource_cache.json");
            RuntimeResourceCache::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RuntimeResourceCache::default(),
        Err(e) => {
            tracing::warn!(path=%path.display(), error=%e, "Failed to read resource_cache.json");
            RuntimeResourceCache::default()
        }
    }
}

/// Save the runtime resource cache to disk.
pub(crate) fn save_resource_cache(work_dir: &std::path::Path, cache: &RuntimeResourceCache) {
    let path = resource_cache_path(work_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string(cache) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&path, &content) {
                tracing::warn!(path=%path.display(), error=%e, "Failed to write resource_cache.json");
            } else {
                tracing::info!(
                    provider_ver = cache.provider_list_version,
                    mcp_ver = cache.mcp_list_version,
                    has_providers = cache.providers.is_some(),
                    "Resource cache saved"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error=%e, "Failed to serialize resource cache");
        }
    }
}

/// Returns Some((client, config)) on success, None on failure (graceful fallback to standalone mode).
pub(crate) async fn connect_gateway_client(
    endpoint: &str,

    agent_id: &str,

    version: &str,

    work_dir: &str,
    outbound_ctrl_capacity: usize,
) -> Option<(
    crate::grpc::client::GatewayGrpcClient,
    crate::grpc::client::AgentHelloConfig,
)> {
    // Read locally-cached resource versions for diff sync.
    let work_dir_path = std::path::Path::new(work_dir);
    let resource_cache = read_resource_cache(work_dir_path);
    let (cached_prov_ver, cached_mcp_ver, cached_search_ver, cached_user_profile_ver) = (
        resource_cache.provider_list_version,
        resource_cache.mcp_list_version,
        resource_cache.search_list_version,
        resource_cache.user_profile_version,
    );

    // ADR-017: Read avatar from agent_config.json to report to Gateway.
    let (avatar, builtin_avatar) =
        match crate::agent_config::load_agent_config(work_dir_path) {
            Ok(Some(cfg)) => (cfg.avatar, cfg.builtin_avatar),
            _ => (None, None),
        };

    match crate::grpc::client::GatewayGrpcClient::connect_and_register(
        endpoint,
        agent_id,
        version,
        cached_prov_ver,
        cached_mcp_ver,
        cached_search_ver,
        cached_user_profile_ver,
        avatar,
        builtin_avatar,
        outbound_ctrl_capacity,
    )
    .await
    {
        Ok((client, config)) => {
            tracing::info!(endpoint = %endpoint, "Connected and registered with Gateway gRPC");
            Some((client, config))
        }

        Err(e) => {
            tracing::warn!(endpoint = %endpoint, error = %e, "Failed to connect to Gateway gRPC");
            None
        }
    }
}

/// Async entry point after tokio runtime is initialized.
///
/// Acts as the top-level phase orchestrator.  All logic lives in the
/// `startup::` sub-modules; this function merely sequences them.
async fn async_main(
    config: RuntimeConfig,
    log_reload_handle: Option<LogReloadHandle>,
) -> Result<()> {
    use crate::startup::{
        phase_a_init_agent, phase_b_init_session, phase_c_spawn_subsystems, phase_d_run,
    };

    // Phase 0: fail-fast validation of timeout configuration.
    config.validate().map_err(crate::error::RuntimeError::Config)?;

    // Phase A: per-agent initialization (package, gateway, provider, tools, embedding).
    let mut agent_ctx = phase_a_init_agent(&config).await?;

    if agent_ctx.grpc_client.is_some() || agent_ctx.mqtt_client.is_some() {
        // ── Gateway mode ────────────────────────────────────────────────────
        // Phase B: per-session initialization (conversation, AgentCore, SessionManager).
        let mut session_ctx = phase_b_init_session(&mut agent_ctx, &config).await?;

        // Phase C: spawn subsystems (chunk_relay, MCP auto-connect, DevMode).
        let handles =
            phase_c_spawn_subsystems(&mut agent_ctx, &mut session_ctx, &config).await?;

        // Phase D: announce ready + run Gateway message loop.
        phase_d_run(&mut agent_ctx, session_ctx, handles, &config, log_reload_handle).await
    } else {
        // ── Standalone mode ──────────────────────────────────────────────────
        use crate::agent::loop_::AgentLoop;
        tracing::info!("Running in standalone mode");
        let (mut agent_loop, _inbound_tx) = AgentLoop::new(
            config.clone(),
            agent_ctx.loaded.manifest.clone(),
            agent_ctx.provider.clone(),
            agent_ctx.active_tools.clone(),
            agent_ctx.budget.clone(),
            agent_ctx.chunk_tx.clone(),
            None, // no conversation session in standalone cold-start
        );

        agent_loop.core.embedding_provider = agent_ctx.emb_provider.clone();
        agent_loop.core.memory_session = Some(agent_ctx.memory_session.clone());
        let work_dir_path = std::path::Path::new(&config.work_dir);
        agent_loop.init_memory_store(work_dir_path);

        let _ = &agent_ctx.gateway_current_provider_id; // unused in standalone
        let mut ctx_builder = agent_ctx
            .context_builder
            .take()
            .expect("context_builder must be Some");
        run_chat_loop(&mut agent_loop, &mut ctx_builder).await
    }
}




/// Build the runtime provider with multi-provider routing support.
///
/// When the manifest declares `providers` + `routing`, constructs a
/// ProviderRegistry and builds a ReliableProvider with fallback chain.
/// Otherwise falls back to a simple single provider.
#[allow(dead_code)]
fn build_runtime_provider(
    manifest: &acowork_core::AgentManifest,

    default_api_key: Option<&str>,

    default_base_url: Option<&str>,
) -> std::sync::Arc<dyn acowork_core::providers::traits::Provider> {
    use crate::providers::registry::{ProviderRegistry, RoutingStrategy};
    use crate::providers::router::{create_provider, infer_protocol_type};

    // If no multi-provider config, return a noop provider.
    // Provider/model now come from resource_cache.providers, not manifest fields.
    if manifest.llm.providers.is_empty() {
        tracing::warn!("No providers configured in manifest, returning noop provider");
        return crate::providers::router::create_noop_provider();
    }

    // Build ProviderRegistry from manifest
    let strategy = manifest
        .llm
        .routing
        .as_ref()
        .map(|r| RoutingStrategy::from_str(&r.strategy))
        .unwrap_or(RoutingStrategy::QualityPriority);
    let registry = ProviderRegistry::with_strategy(strategy);

    // Register each provider from manifest
    for (name, config) in &manifest.llm.providers {
        let api_key = config.api_key_ref.as_deref().or(default_api_key);
        let base_url = config.base_url.as_deref().or(default_base_url);
        let provider = create_provider(name, &infer_protocol_type(name), api_key, base_url, None);
        let models = vec![config.model.clone()];
        registry.register_provider(name, provider, models);
    }

    // Use the first provider as primary for the ReliableProvider
    if let Some((primary_name, _)) = manifest.llm.providers.iter().next() {
        let primary_model = manifest
            .llm
            .providers
            .get(primary_name)
            .map(|c| c.model.clone())
            .unwrap_or_default();
        match registry.build_reliable_provider(primary_name, &primary_model) {
            Some(reliable) => {
                tracing::info!(
                    primary = %primary_name,
                    model = %primary_model,
                    strategy = %strategy,
                    "Built ReliableProvider with fallback chain"
                );
                std::sync::Arc::new(reliable)
            }
            None => {
                tracing::warn!("Failed to build ReliableProvider, falling back to noop provider");
                crate::providers::router::create_noop_provider()
            }
        }
    } else {
        tracing::warn!("Provider registry is empty, returning noop provider");
        crate::providers::router::create_noop_provider()
    }
}

/// Run interactive stdin chat loop
async fn run_chat_loop(
    agent_loop: &mut crate::agent::loop_::AgentLoop,

    context_builder: &mut crate::agent::context::ContextBuilder,
) -> Result<()> {
    use std::io::{self, BufRead, Write};
    println!("ACowork Agent Runtime — type messages and press Enter (Ctrl+C to exit)");
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

        match agent_loop.run(trimmed, context_builder, None, None, Some(trimmed)).await {
            Ok(response) => {
                println!(
                    "

--- Agent ---

{response}

"
                );
            }

            Err(e) => {
                tracing::error!(error = %e, "Agent loop error");
                println!(
                    "

--- Error ---

{e}

"
                );
            }
        }

        stdout.flush().ok();
    }

    Ok(())
}

/// Run Gateway message loop — receives messages from Gateway and routes them.
///
/// This loop is **pure routing**: it never blocks on any Session's execution.
/// Messages are forwarded to the appropriate SessionHandle's inbound channel
/// and the loop immediately returns to recv the next message.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) async fn run_gateway_loop(
    session_manager: &mut SessionManager,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    mut gateway_query_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<(u64, acowork_core::proto::server_message::Payload)>,
    >,
    work_dir: String,
    agent_id_for_reconnect: String,
    version_for_reconnect: String,
    log_reload_handle: Option<LogReloadHandle>,
    resolver: crate::tools::workspace_resolver::SharedResolver,
    _session_idle_timeout_secs: u64,
    max_sessions: usize,
    timeouts: acowork_core::Timeouts,
    mut mcp_config_rx: tokio::sync::watch::Receiver<()>,
    mut mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    >,
    mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    mut mqtt_dispatch_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, crate::agent::inbound::InboundMessage)>>,
) -> Result<()> {
    // Retrieve the provider name for budget queries
    let budget_provider = session_manager.provider_name();
    tracing::info!("Gateway message loop started (pure routing mode)");
    use acowork_core::proto;
    use acowork_core::proto::server_message::Payload as ServerPayload;

    // Main message loop — receive messages from Gateway and route them.
    // Also polls the gateway query channel for HTTP→Runtime request-response
    // queries (QueryConfig, Memory API), receives MCP config change
    // notifications via watch channel (event-driven, no polling), and
    // receives initial MCP auto-connect results from the background task
    // started in async_main (applied asynchronously so the loop starts immediately).
    loop {
        if let Some(ref mut mq_rx) = gateway_query_rx {
            tokio::select! {
                // ADR-033: MQTT control message dispatch.
                dispatch_result = async {
                    match &mut mqtt_dispatch_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((session_id, msg)) = dispatch_result
                        && let Some(session) = session_manager.get_session(&session_id)
                    {
                        let _ = session.send_inbound(msg);
                    }
                }

                recv_result = grpc_client.recv_message() => {
                    match process_gateway_recv(
                        recv_result,
                        session_manager,
                        grpc_client,
                        &work_dir,
                        &resolver,
                        &agent_id_for_reconnect,
                        &version_for_reconnect,
                        &budget_provider,
                        &log_reload_handle,
                        max_sessions,
                        &timeouts,
                        &mcp_runtime_tx,
                    ).await {
                        LoopAction::Continue => continue,
                        LoopAction::Break => break,
                    }
                }

                // Initial MCP auto-connect result — received from the background
                // connect task spawned in async_main.  Applied asynchronously
                // so the Gateway message loop can start immediately.
                mcp_result = async {
                    match &mut mcp_startup_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((registry, wrappers, specs, failures)) = mcp_result {
                        session_manager.apply_mcp_connection_result(
                            registry, wrappers, specs, failures,
                        );
                    }
                    mcp_startup_rx = None;
                }

                // Runtime MCP connect result — received from background connect
                // tasks spawned by RuntimeConfigUpdate or mcp_config_rx handlers.
                mcp_runtime_result = mcp_runtime_rx.recv() => {
                    if let Some((registry, wrappers, specs, failures)) = mcp_runtime_result {
                        session_manager.apply_mcp_connection_result(
                            registry, wrappers, specs, failures,
                        );
                    }
                }

                // MCP config change notification — triggered by mcp_install
                // / mcp_uninstall tools via McpConfigNotifier::notify().
                // Event-driven, zero-latency — replaces periodic polling.
                // Connection is spawned in background to avoid blocking the
                // Gateway message loop (up to 30s per server timeout).
                _ = mcp_config_rx.changed() => {
                    tracing::info!("MCP config change notification — reconnecting MCP servers (background)");
                    let merged = crate::agent_config::load_merged_mcp_configs(
                        std::path::Path::new(&work_dir),
                    );
                    let tx = mcp_runtime_tx.clone();
                    tokio::spawn(async move {
                        let (registry, failures) =
                            acowork_mcp::client::McpRegistry::connect_all(&merged)
                                .await
                                .expect("connect_all is non-fatal and should never fail");
                        let registry = std::sync::Arc::new(registry);
                        let mut wrappers = Vec::new();
                        let mut specs = Vec::new();
                        for prefixed_name in registry.tool_names() {
                            if let Some(def) = registry.get_tool_def(&prefixed_name) {
                                let wrapper = acowork_mcp::wrapper::McpToolWrapper::new(
                                    prefixed_name.clone(), def, registry.clone(),
                                );
                                use acowork_core::tools::traits::Tool;
                                let tool_spec = wrapper.spec();
                                let serialized = serde_json::to_value(&tool_spec).unwrap_or_default();
                                specs.push((tool_spec.name.clone(), serialized));
                                wrappers.push(wrapper);
                            }
                        }
                        let _ = tx.send((registry, wrappers, specs, failures)).await;
                    });
                }

                query_opt = mq_rx.recv() => {
                    match query_opt {
                        Some((request_id, payload)) => {
                            // Handle QueryConfig inline (no Grafeo needed)
                            if let ServerPayload::QueryConfig(_q) = &payload {
                                // ADR-012: Per-session model — use cached LLM config or empty.
                                let current_model = session_manager.current_model_name()
                                    .unwrap_or_default();
                                let current_provider = Some(session_manager.provider_name());
                                let overrides = &session_manager.runtime_overrides;
                                // Read MCP config from separate agent_mcp.json (merged: catalog + local).
                                let mcp_json: Vec<String> = crate::agent_config::load_merged_mcp_configs(
                                    std::path::Path::new(&work_dir),
                                )
                                .iter()
                                .map(|s| serde_json::to_string(s).unwrap_or_default())
                                .collect();
                                // ADR-017: Include avatar config from agent_config.json.
                                let agent_cfg = crate::agent_config::load_agent_config(
                                    std::path::Path::new(&work_dir),
                                )
                                .unwrap_or_default()
                                .unwrap_or_default();

                                // Build merged builtin tools list: load from disk,
                                // then merge dynamically registered tools (from
                                // SidecarEndpointUpdate) so ConfigSnapshot reflects
                                // tools that were not available at startup (e.g.
                                // codebase when LSP Relay became ready after
                                // AgentHello).
                                let mut merged_tools: Vec<crate::agent_config::AgentToolEntry> =
                                    crate::agent_config::load_agent_tools_config(
                                        std::path::Path::new(&work_dir),
                                    )
                                    .unwrap_or_default()
                                    .map(|cfg| cfg.tools)
                                    .unwrap_or_default();
                                for (name, enabled) in session_manager.dynamic_builtin_tool_names() {
                                    if !merged_tools.iter().any(|e| e.name == name) {
                                        merged_tools.push(
                                            crate::agent_config::AgentToolEntry::new(name, enabled),
                                        );
                                    }
                                }
                                // context_recall is a platform-protected tool and
                                // must not be exposed to the frontend tool panel.
                                let exposed_tools: Vec<_> = merged_tools
                                    .into_iter()
                                    .filter(|e| e.name != "context_recall")
                                    .collect();
                                let builtin_tools_all_json =
                                    serde_json::to_string(&exposed_tools).unwrap_or_default();
                                let builtin_tools_enabled_json = {
                                    let enabled: Vec<String> = exposed_tools
                                        .iter()
                                        .filter(|e| e.enabled)
                                        .map(|e| e.name.clone())
                                        .collect();
                                    serde_json::to_string(&enabled).unwrap_or_default()
                                };

                                let snapshot = proto::client_message::Payload::ConfigSnapshot(
                                    proto::ConfigSnapshot {
                                        request_id: String::new(),
                                        model: Some(current_model),
                                        provider: current_provider,
                                        max_output_tokens: overrides.max_output_tokens,
                                        max_iterations: overrides.max_iterations,
                                        temperature: overrides.temperature,
                                        system_prompt_override: overrides.system_prompt_override.clone(),
                                        shell_approval_threshold: overrides.shell_approval_threshold.clone(),
                                        mcp_servers_json: mcp_json,
                                        search_config_json: crate::agent_config::load_agent_search_config(
                                            std::path::Path::new(&work_dir),
                                        )
                                        .unwrap_or_default()
                                        .and_then(|cfg| serde_json::to_string(&cfg).ok()),
                                        avatar: agent_cfg.avatar.clone(),
                                        builtin_avatar: agent_cfg.builtin_avatar.clone(),
                                        max_sessions: agent_cfg.max_sessions.map(|v| v as u64),
                                        context_window: agent_cfg.context_window,
                                    approval_timeout_secs: agent_cfg.approval_timeout_secs,
                                    builtin_tools_enabled_json: Some(builtin_tools_enabled_json),
                                    builtin_tools_all_json: Some(builtin_tools_all_json),
                                    // ADR-032 C4b: compression trigger mode from runtime overrides
                                    tool_result_compression_mode: overrides.tool_result_compression_mode.clone(),
                                    // ADR-032 C4a: tool-result soft compression threshold
                                    // from runtime overrides (boot-only semantics on
                                    // the runtime side; see proto comments).
                                    tool_result_soft_threshold_chars: overrides.tool_result_soft_threshold_chars.map(|v| v as u64),
                                },
                                );
                                let response = proto::ClientMessage {
                                    request_id,
                                    payload: Some(snapshot),
                                };
                                let outbound = grpc_client.outbound_ctrl_sender();
                                if let Err(e) = outbound.send(response).await {
                                    tracing::error!("Failed to send ConfigSnapshot: {}", e);
                                }
                            } else {
                                // Spawn to a separate task so Grafeo queries don't block

                                // the select! loop from processing Gateway messages (session

                                // refresh, etc.). The task holds cloned Arc/Sender handles.
                                let store_opt = session_manager.core().memory_store().cloned();
                                let embed_dim = session_manager.core().embedding_provider.as_ref().map(|p| p.dimension() as u64).unwrap_or(0);
                                let outbound = grpc_client.outbound_ctrl_sender();

                                // Handle GetLatestSessionQuery inline — reads the cached
                                // latest session from SessionManager (populated by the
                                // startup scan). No disk I/O.
                                if let ServerPayload::GetLatestSessionQuery(ref q) = payload {
                                    let result = if let Some((session_id, title)) =
                                        session_manager.latest_session()
                                    {
                                        proto::LatestSessionResult {
                                            request_id: q.request_id.clone(),
                                            found: true,
                                            session_id,
                                            title: title.unwrap_or_default(),
                                            created_at: String::new(),
                                        }
                                    } else {
                                        proto::LatestSessionResult {
                                            request_id: q.request_id.clone(),
                                            found: false,
                                            ..Default::default()
                                        }
                                    };
                                    let response = proto::ClientMessage {
                                        request_id,
                                        payload: Some(
                                            proto::client_message::Payload::LatestSessionResult(
                                                result,
                                            ),
                                        ),
                                    };
                                    if let Err(e) = outbound.send(response).await {
                                        tracing::error!(
                                            "Failed to send LatestSessionResult: {}",
                                            e
                                        );
                                    }
                                // Handle GetSessionStateQuery inline — the snapshot is
                                // a cheap RwLock read that never blocks.
                                } else if let ServerPayload::GetSessionStateQuery(ref q) = payload {
                                    let snapshot = session_manager.snapshot_session_state(&q.session_id);
                                    let result = if let Some(snap) = snapshot {
                                        proto::SessionStateResult {
                                            request_id: q.request_id.clone(),
                                            found: true,
                                            session_id: snap.session_id,
                                            status_json: snap.status_json,
                                            model: snap.model.unwrap_or_default(),
                                            provider: snap.provider.unwrap_or_default(),
                                            workspace_id: snap.workspace_id.unwrap_or_default(),
                                            ratio: snap.ratio.unwrap_or(0.0),
                                            reasoning_effort: snap.reasoning_effort.unwrap_or_default(),
                                            temperature: snap.temperature.unwrap_or(0.0),
                                            has_temperature: snap.temperature.is_some(),
                                            todos_json: snap.todos_json.unwrap_or_default(),
                                            context_usage_json: snap.context_usage_json.unwrap_or_default(),
                                        }
                                    } else {
                                        proto::SessionStateResult {
                                            request_id: q.request_id.clone(),
                                            found: false,
                                            session_id: q.session_id.clone(),
                                            ..Default::default()
                                        }
                                    };
                                    let response = proto::ClientMessage {
                                        request_id,
                                        payload: Some(proto::client_message::Payload::SessionStateResult(result)),
                                    };
                                    if let Err(e) = outbound.send(response).await {
                                        tracing::error!("Failed to send SessionStateResult: {}", e);
                                    }
                                } else {
                                    tokio::spawn(spawn_memory_query_handler(

                                    store_opt,
                                    embed_dim,
                                    outbound,
                                    request_id,
                                    payload,

                                ));
                                }
                            }
                        }
                        None => {
                            tracing::warn!("Gateway query channel closed unexpectedly");
                            gateway_query_rx = None;
                        }
                    }
                }
            }
        } else {
            match process_gateway_recv(
                grpc_client.recv_message().await,
                session_manager,
                grpc_client,
                &work_dir,
                &resolver,
                &agent_id_for_reconnect,
                &version_for_reconnect,
                &budget_provider,
                &log_reload_handle,
                max_sessions,
                &timeouts,
                &mcp_runtime_tx,
            )
            .await
            {
                LoopAction::Continue => continue,
                LoopAction::Break => break,
            }
        }
    }

    tracing::info!("Gateway message loop ended");

    // Explicitly close the Grafeo memory store so all pending WAL
    // entries are checkpointed to the .grafeo file on disk.  Relying
    // solely on Drop is fragile when the process is terminated via
    // Ctrl+C or the desktop app kills the child process.
    if let Some(store) = session_manager.core().memory_store() {
        if let Err(e) = store.close() {
            tracing::warn!(
                error = %e,
                "Failed to close Grafeo memory store during shutdown (non-fatal)"
            );
        } else {
            tracing::info!("Grafeo memory store closed (checkpointed to disk)");
        }
    }

    Ok(())
}

// ── Loop control ────────────────────────────────────────────────────────────

/// Return value for process_gateway_recv to control loop flow.
#[allow(dead_code)]
enum LoopAction {
    Continue,

    Break,
}

// ── Gateway message processor ───────────────────────────────────────────────

/// Process a single recv_message() result from the Gateway gRPC connection.
/// Returns LoopAction::Continue to keep looping, LoopAction::Break to exit.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
async fn process_gateway_recv(
    recv_result: std::result::Result<
        Option<acowork_core::protocol::GatewayResponse>,
        acowork_core::error::AcoworkError,
    >,
    session_manager: &mut SessionManager,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    work_dir: &str,
    resolver: &crate::tools::workspace_resolver::SharedResolver,
    agent_id_for_reconnect: &str,
    version_for_reconnect: &str,
    budget_provider: &str,
    log_reload_handle: &Option<LogReloadHandle>,
    _max_sessions: usize,
    timeouts: &acowork_core::Timeouts,
    mcp_runtime_tx: &tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
) -> LoopAction {
    use acowork_core::protocol::GatewayResponse;
    match recv_result {
        Ok(Some(response)) => {
            tracing::debug!("Received Gateway message: {:?}", response);
            match response {
                GatewayResponse::IntentReceived {
                    from,
                    action,
                    params,
                    command: _,
                } => {
                    tracing::info!("Received intent from {}: {}", from, action);

                    // ── Session management actions (no session_id required) ──
                    // These handle session lifecycle and carry their own target
                    // resolution.  Handle them BEFORE the routing block so
                    // create_session / list_sessions don't fail the
                    // require_session_id check.

                    if action == "list_sessions" {
                        handle_list_sessions(work_dir, grpc_client, &params, session_manager)
                            .await;
                        return LoopAction::Continue;
                    }

                    if action == "get_session_messages" {
                        handle_get_session_messages(work_dir, grpc_client, &params, session_manager).await;
                        return LoopAction::Continue;
                    }

                    if action == "create_session" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // ADR-012: Accept initial per-session metadata from frontend.
                        let initial_workspace = params.get("workspace_id").and_then(|v| v.as_str());
                        let initial_model = params.get("model").and_then(|v| v.as_str());
                        let initial_provider = params.get("provider").and_then(|v| v.as_str());

                        match session_manager
                            .create_frontend_session(initial_workspace, initial_model, initial_provider)
                            .await
                        {
                            Ok(new_session_id) => {
                                tracing::info!(new_session_id = %new_session_id, "Created new session via Gateway request");
                                let data = serde_json::json!({ "session_id": new_session_id });
                                send_session_response(grpc_client, &request_id, data).await;
                            }
                            Err(e) => {
                                tracing::error!("Failed to create session: {}", e);
                                let data = serde_json::json!({ "error": format!("Failed to create session: {}", e) });
                                send_session_response(grpc_client, &request_id, data).await;
                            }
                        }

                        return LoopAction::Continue;
                    }

                    if action == "activate_session" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
                            Some(sid) if !sid.is_empty() => sid.to_string(),

                            _ => {
                                let data = serde_json::json!({ "error": "Missing or empty session_id parameter" });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                        };

                        // Ensure session is in memory — lazy-resume from disk if needed.
                        // Uses the unified session recovery path (same as chat_message,
                        // model_switch, etc.) to avoid duplicated resume logic.
                        if let Err(e) = session_manager
                            .ensure_session_in_memory(&session_id, std::path::Path::new(work_dir))
                            .await
                        {
                            tracing::warn!(session_id = %session_id, error = %e, "Cannot activate session");
                            let data = serde_json::json!({ "error": format!("Session not found: {}", session_id) });
                            send_session_response(grpc_client, &request_id, data).await;
                            return LoopAction::Continue;
                        }

                        // ADR-035 Phase 3: EnableNotify removed — push drives
                        // all streaming, no activation-time notify needed.

                        // Read session metadata from JSONL to return model/provider/workspace_id
                        // to the frontend in the activation response, so it can populate the UI
                        // immediately without waiting for a WS event.
                        let (session_model, session_provider, session_workspace_id) = {
                            let conversations_dir =
                                std::path::Path::new(work_dir).join("conversations");
                            // ADR-024: read from per-session meta file.
                            match crate::conversation::read_session_meta(&conversations_dir, &session_id)
                            {
                                Ok(meta) => (meta.model, meta.provider, meta.workspace_id),
                                Err(_) => (None, None, None),
                            }
                        };

                        let data = serde_json::json!({
                            "session_id": session_id,
                            "activated": true,
                            "model": session_model,
                            "provider": session_provider,
                            "workspace_id": session_workspace_id,
                        });
                        send_session_response(grpc_client, &request_id, data).await;
                        return LoopAction::Continue;
                    }

                    if action == "deactivate_session" {
                        // ADR-035 Phase 3: deactivate_session is now a no-op —
                        // EnableNotify/DisableNotify removed, push drives all
                        // streaming uniformly. Validate session_id for logging.
                        let _session_id = match params.get("session_id").and_then(|v| v.as_str()) {
                            Some(sid) if !sid.is_empty() => sid.to_string(),
                            _ => {
                                tracing::warn!("deactivate_session: missing or empty session_id");
                                return LoopAction::Continue;
                            }
                        };
                        return LoopAction::Continue;
                    }

                    if action == "close_session" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
                            Some(sid) if !sid.is_empty() => sid.to_string(),
                            _ => {
                                let data = serde_json::json!({ "error": "Missing or empty session_id parameter" });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                        };

                        // Close the session: disable notifications, trigger distillation,
                        // and free in-memory resources (JSONL preserved).
                        if let Err(e) = session_manager.close_session(&session_id).await {
                            tracing::warn!("Failed to close session {}: {}", session_id, e);
                        }

                        let data = serde_json::json!({
                            "closed": true,
                            "session_id": session_id,
                        });
                        send_session_response(grpc_client, &request_id, data).await;
                        return LoopAction::Continue;
                    }

                    if action == "delete_session" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
                            Some(sid) if !sid.is_empty() => sid.to_string(),

                            _ => {
                                let data = serde_json::json!({ "error": "Missing or empty session_id parameter" });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                        };

                        session_manager.delete_session(&session_id).await;

                        let data = serde_json::json!({
                            "deleted": true,
                            "session_id": session_id,
                        });
                        send_session_response(grpc_client, &request_id, data).await;
                        return LoopAction::Continue;
                    }

                    if action == "update_session_title" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let title = match params.get("title").and_then(|v| v.as_str()) {
                            Some(t) if !t.trim().is_empty() => t.trim().to_string(),

                            _ => {
                                let data = serde_json::json!({ "error": "Missing or empty title parameter" });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                        };
                        let target_sid = match params.get("session_id").and_then(|v| v.as_str()) {
                            Some(sid) if !sid.is_empty() => sid.to_string(),
                            _ => {
                                let data = serde_json::json!({ "error": "Missing or empty session_id parameter" });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                        };
                        // Ensure session is in memory (may have been evicted or Runtime restarted).
                        if let Err(e) = session_manager
                            .ensure_session_in_memory(&target_sid, std::path::Path::new(work_dir))
                            .await
                        {
                            tracing::warn!(session_id = %target_sid, error = %e, "Cannot update session title");
                            let data = serde_json::json!({ "error": format!("Session not found: {}", target_sid) });
                            send_session_response(grpc_client, &request_id, data).await;
                            return LoopAction::Continue;
                        }

                        if let Err(e) = session_manager.send_to_session(
                            &target_sid,
                            SessionMessage::UpdateSessionTitle {
                                title: title.clone(),
                            },
                        ) {
                            tracing::warn!("Failed to route update_session_title: {}", e);
                            let data = serde_json::json!({ "error": format!("Session not found: {}", target_sid) });
                            send_session_response(grpc_client, &request_id, data).await;
                        } else {
                            let data = serde_json::json!({
                                "session_id": target_sid,
                                "title": title,
                                "updated": true,
                            });
                            send_session_response(grpc_client, &request_id, data).await;
                        }

                        return LoopAction::Continue;
                    }

                    // ── All remaining actions MUST carry session_id ──
                    // Determine target session: every message MUST carry session_id.
                    let target_session_id = match SessionManager::require_session_id(&params) {
                        Ok(sid) => sid,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                action = %action,
                                "Missing session_id in message, skipping"
                            );
                            return LoopAction::Continue;
                        }
                    };

                    // Handle model_switch: ADR-012 — per-session model routing.
                    // Only the targeted session receives the model switch.
                    // Model persistence is handled by SessionTask (JSONL metadata).
                    if action == "model_switch" {
                        if let Some(model) = params.get("model").and_then(|v| v.as_str()) {
                            let provider = params.get("provider").and_then(|v| v.as_str());
                            // ADR-012: Extract session_id from params (passed by Gateway).
                            // Falls back to target_session_id (from message routing) if not specified.
                            let switch_session_id = params
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&target_session_id);

                            // Ensure session is in memory after potential Runtime restart.
                            if let Err(e) = session_manager
                                .ensure_session_in_memory(switch_session_id, std::path::Path::new(work_dir))
                                .await
                            {
                                tracing::warn!(session_id = %switch_session_id, error = %e, "Cannot route model_switch");
                                return LoopAction::Continue;
                            }

                            if let Err(e) = session_manager.route_model_switch(
                                switch_session_id,
                                model.to_string(),
                                provider.map(|s| s.to_string()),
                            ) {
                                tracing::warn!(
                                    session_id = %switch_session_id,
                                    model = %model,
                                    error = %e,
                                    "Failed to route model_switch to session"
                                );
                            } else {
                                tracing::info!(
                                    session_id = %switch_session_id,
                                    model = %model,
                                    provider = ?provider,
                                    "Model switched via model_switch (ADR-012: per-session)"
                                );
                            }
                        } else {
                            tracing::warn!("model_switch message missing 'model' field, ignoring");
                        }

                        return LoopAction::Continue;
                    }

                    // Handle reasoning_effort: per-session reasoning depth override.
                    // The frontend sends this via WebSocket; Gateway forwards as IntentReceived.
                    if action == "reasoning_effort" {
                        if let Some(effort) = params.get("effort").and_then(|v| v.as_str()) {
                            let effort_session_id = params
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&target_session_id);

                            // Ensure session is in memory after potential Runtime restart.
                            if let Err(e) = session_manager
                                .ensure_session_in_memory(effort_session_id, std::path::Path::new(work_dir))
                                .await
                            {
                                tracing::warn!(session_id = %effort_session_id, error = %e, "Cannot route reasoning_effort");
                                return LoopAction::Continue;
                            }

                            if let Err(e) = session_manager.route_reasoning_effort(
                                effort_session_id,
                                effort.to_string(),
                            ) {
                                tracing::warn!(
                                    session_id = %effort_session_id,
                                    effort = %effort,
                                    error = %e,
                                    "Failed to route reasoning_effort to session"
                                );
                            } else {
                                tracing::info!(
                                    session_id = %effort_session_id,
                                    effort = %effort,
                                    "Reasoning effort override applied"
                                );
                            }
                        } else {
                            tracing::warn!("reasoning_effort message missing 'effort' field, ignoring");
                        }

                        return LoopAction::Continue;
                    }

                    // Handle interrupt: route directly to the target session's
                    // AgentLoop inbound channel via SessionHandle::send_inbound,
                    // BYPASSING SessionTask's SessionMessage loop — the latter
                    // is blocked inside `agent_loop.run().await` whenever the
                    // loop is active, so routing via SessionMessage would
                    // deadlock until the current iteration finishes.
                    if action == "interrupt" {
                        let reason = params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        tracing::info!(reason = %reason, session_id = %target_session_id, "Routing stop to session");

                        tracing::warn!(
                            reason = %reason,
                            session_id = %target_session_id,
                            "gRPC removed (ADR-033): fire_urgent_stop is no-op"
                        );

                        // 2. Deliver InboundMessage::Stop to the specific session's
                        //    AgentLoop inbox — this sets the stop flag that poll_stop()
                        //    and drain_inbound_queue() check, ensuring the loop exits
                        //    cleanly after the current operation is aborted.
                        match session_manager.get_session(&target_session_id) {
                            Some(handle) => {
                                if let Err(e) = handle.send_inbound(InboundMessage::Stop { reason })
                                {
                                    tracing::warn!("Failed to deliver stop to AgentLoop: {}", e);
                                }
                            }

                            None => {
                                tracing::warn!(session_id = %target_session_id, "Stop target session not found");
                            }
                        }

                        return LoopAction::Continue;
                    }

                    if action == "continue_execution" {
                        let reason = params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("user_requested")
                            .to_string();
                        tracing::info!(reason = %reason, session_id = %target_session_id, "Routing continue_execution to session");

                        // Same deadlock-avoidance as `interrupt`: go directly
                        // into the AgentLoop's inbound channel so the pause
                        // recv loop (awaiting ContinueExecution) is unblocked
                        // immediately.
                        match session_manager.get_session(&target_session_id) {
                            Some(handle) => {
                                if let Err(e) = handle.send_inbound(
                                    InboundMessage::ContinueExecution {
                                        // ADR-034: populate session_id for Phase 2 routing.
                                        session_id: target_session_id.clone(),
                                        reason,
                                    },
                                ) {
                                    tracing::warn!(
                                        "Failed to deliver continue signal to AgentLoop: {}",
                                        e
                                    );
                                }
                            }

                            None => {
                                tracing::warn!(session_id = %target_session_id, "Continue target session not found");
                            }
                        }

                        return LoopAction::Continue;
                    }

                    if action == "approval_decision" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let approved = params
                            .get("approved")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let allow_all_session = params
                            .get("allow_all_session")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let reason = params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        tracing::info!(

                            request_id = %request_id,
                            approved,
                            allow_all_session,
                            session_id = %target_session_id,
                            "Routing approval_decision to session"

                        );

                        // Route directly to AgentLoop's inbound channel to
                        // unblock `await_approval_decision()` immediately.
                        match session_manager.get_session(&target_session_id) {
                            Some(handle) => {
                                if let Err(e) =
                                    handle.send_inbound(InboundMessage::ApprovalDecision {
                                        request_id,
                                        approved,
                                        allow_all_session,
                                        reason,
                                        // ADR-034: populate session_id for Phase 2 routing.
                                        session_id: target_session_id.clone(),
                                    })
                                {
                                    tracing::warn!(
                                        "Failed to deliver approval decision to AgentLoop: {}",
                                        e
                                    );
                                }
                            }

                            None => {
                                tracing::warn!(session_id = %target_session_id, "Approval decision target session not found");
                            }
                        }

                        return LoopAction::Continue;
                    }

                    if action == "question_answer" {
                        let request_id = params
                            .get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let answer = params
                            .get("answer")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        tracing::info!(
                            request_id = %request_id,
                            answer_preview = %answer.chars().take(80).collect::<String>(),
                            session_id = %target_session_id,
                            "Routing question_answer to session"
                        );

                        // Route directly to AgentLoop's inbound channel to
                        // unblock `await_question_answer()` immediately.
                        // Ensure session is in memory after potential Runtime restart.
                        if let Err(e) = session_manager
                            .ensure_session_in_memory(&target_session_id, std::path::Path::new(work_dir))
                            .await
                        {
                            tracing::warn!(session_id = %target_session_id, error = %e, "Cannot route question_answer");
                            return LoopAction::Continue;
                        }

                        match session_manager.get_session(&target_session_id) {
                            Some(handle) => {
                                if let Err(e) =
                                    handle.send_inbound(InboundMessage::QuestionAnswer {
                                        request_id,

                                        answer,
                                        // ADR-034: populate session_id for Phase 2 routing.
                                        session_id: target_session_id.clone(),
                                    })
                                {
                                    tracing::warn!(
                                        "Failed to deliver question answer to AgentLoop: {}",
                                        e
                                    );
                                }
                            }

                            None => {
                                tracing::warn!(session_id = %target_session_id, "Question answer target session not found");
                            }
                        }

                        return LoopAction::Continue;
                    }

                    if action == "compact_context" {
                        tracing::info!(
                            session_id = %target_session_id,
                            "Routing compact_context to session"
                        );

                        // Ensure session is in memory after potential Runtime restart.
                        if let Err(e) = session_manager
                            .ensure_session_in_memory(&target_session_id, std::path::Path::new(work_dir))
                            .await
                        {
                            tracing::warn!(session_id = %target_session_id, error = %e, "Cannot route compact_context");
                            return LoopAction::Continue;
                        }

                        if let Err(e) = session_manager
                            .send_to_session(&target_session_id, SessionMessage::CompactContext)
                        {
                            tracing::warn!(
                                session_id = %target_session_id,
                                error = %e,
                                "Failed to route compact_context to session"
                            );
                        }
                        return LoopAction::Continue;
                    }

                    if action == "compress_action" {
                        tracing::info!(
                            session_id = %target_session_id,
                            "Routing compress_action to session"
                        );

                        // Ensure session is in memory after potential Runtime restart.
                        if let Err(e) = session_manager
                            .ensure_session_in_memory(&target_session_id, std::path::Path::new(work_dir))
                            .await
                        {
                            tracing::warn!(session_id = %target_session_id, error = %e, "Cannot route compress_action");
                            return LoopAction::Continue;
                        }

                        // Parse the action type from params
                        let compress_type = params
                            .get("compress_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("compress_tool_results");

                        let session_msg = match compress_type {
                            "compress_summary" => {
                                SessionMessage::CompressAction(
                                    crate::agent::loop_::CompressionAction::CompressSummary,
                                )
                            }
                            _ => {
                                SessionMessage::CompressAction(
                                    crate::agent::loop_::CompressionAction::CompressToolResults,
                                )
                            }
                        };

                        if let Err(e) = session_manager
                            .send_to_session(&target_session_id, session_msg)
                        {
                            tracing::warn!(
                                session_id = %target_session_id,
                                error = %e,
                                "Failed to route compress_action to session"
                            );
                        }
                        return LoopAction::Continue;
                    }

                    // Budget pre-check: skip processing if budget is exhausted.
                    if let Ok((remaining_tokens, _)) =
                        grpc_client.query_budget(budget_provider).await
                        && remaining_tokens == 0
                    {
                        tracing::warn!(
                            "Budget exhausted for provider={}, skipping message from {}",
                            budget_provider,
                            from
                        );
                        let error_params = serde_json::json!({
                            "content": "Budget exhausted — cannot process this message",
                            "message_id": params.get("message_id")

                                .and_then(|v| v.as_str())

                                .unwrap_or("unknown"),
                        });
                        let _ = grpc_client
                            .send_intent(&from, "agent_error", error_params, false)
                            .await;
                        return LoopAction::Continue;
                    }

                    // Extract message content from params
                    let content = params
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // If a command is specified, resolve skill instructions.
                    // Instructions are passed separately (via ContextBuilder / system prompt)
                    // instead of being prepended to the user message, making them
                    // visible in the debug panel's context snapshot.
                    // ADR-034: skill_registry removed from context — gRPC path only.
                    let skill_instructions: Option<String> = None;
                    let message_id = params
                        .get("message_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!("msg-{}", chrono::Utc::now().timestamp_millis())
                        });

                    // Extract document references if present (for doc_reader integration)
                    let documents: Option<Vec<serde_json::Value>> = params
                        .get("documents")
                        .and_then(|v| v.as_array()).cloned();

                    // Extract multimodal content_parts if present (e.g. text + image_url)
                    let content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>> =
                        params
                            .get("content_parts")
                            .and_then(|v| serde_json::from_value(v.clone()).ok());

                    // Extract attached_context (files/selections added by user)
                    let attached_context: Option<Vec<acowork_core::protocol::AttachedContextItem>> =
                        params
                            .get("attached_context")
                            .and_then(|v| serde_json::from_value(v.clone()).ok());

                    // Ensure session is in memory after potential Runtime restart.
                    // This is the unified lazy-resume path — if the Runtime crashed and
                    // was restarted, or the session was evicted due to idle timeout,
                    // this call restores the session from its JSONL file on disk.
                    if let Err(e) = session_manager
                        .ensure_session_in_memory(&target_session_id, std::path::Path::new(work_dir))
                        .await
                    {
                        tracing::error!(
                            session_id = %target_session_id,
                            error = %e,
                            "Failed to ensure session in memory for chat_message"
                        );
                        let error_params = serde_json::json!({
                            "content": format!("Session not found: {}", target_session_id),
                            "message_id": message_id,
                        });
                        let _ = grpc_client
                            .send_intent(&from, "agent_error", error_params, false)
                            .await;
                        return LoopAction::Continue;
                    }

                    // Pure routing: send to session's inbound channel, immediately return
                    if let Err(e) = session_manager.send_to_session(
                        &target_session_id,
                        SessionMessage::ChatMessage {
                            content,
                            message_id: message_id.clone(),
                            skill_instructions,
                            documents,
                            content_parts,
                            attached_context,
                        },
                    ) {
                        tracing::error!(
                            "Failed to route message to session {}: {}",
                            target_session_id,
                            e
                        );
                        let error_params = serde_json::json!({
                            "content": format!("Session not found: {}", target_session_id),
                            "message_id": message_id,
                        });
                        let _ = grpc_client
                            .send_intent(&from, "agent_error", error_params, false)
                            .await;
                    }

                    LoopAction::Continue
                }

                GatewayResponse::ProviderListUpdate {
                    provider_list,
                    provider_list_version,
                    provider_key_vault,
                } => {
                    tracing::info!(
                        provider_count = provider_list.len(),
                        version = provider_list_version,
                        key_count = provider_key_vault.len(),
                        "Received ProviderListUpdate at runtime — updating global provider cache"
                    );

                    // Update the shared AgentCore via SessionManager — sessions
                    // will pick up new provider/key data on demand. Clone the
                    // provider list so we can also persist it to disk below.
                    let providers_for_cache = provider_list.clone();
                    session_manager.update_global_provider_list(
                        provider_list,
                        provider_list_version,
                        provider_key_vault,
                    );

                    // Persist provider_list + version to resource_cache.json
                    // for next-startup AgentHello diff sync. Provider API keys
                    // are NEVER persisted (kept only in the in-memory vault).
                    let mut cache = read_resource_cache(std::path::Path::new(&work_dir));
                    cache.provider_list_version = provider_list_version;
                    cache.providers = Some(providers_for_cache);
                    save_resource_cache(std::path::Path::new(&work_dir), &cache);

                    LoopAction::Continue
                }

                GatewayResponse::SearchConfigDelivery {
                    search_key_vault,
                    search_list,
                    search_list_version,
                } => {
                    tracing::info!(
                        provider_count = search_list.len(),
                        key_count = search_key_vault.len(),
                        version = search_list_version,
                        "Received SearchConfigDelivery at runtime — caching search config"
                    );

                    // Cache in SessionManager for ConfigSnapshot queries
                    session_manager.update_search_config(search_key_vault, search_list);

                    // Persist search_list_version to resource_cache.json
                    // for next startup's AgentHello diff sync.
                    let mut cache = read_resource_cache(std::path::Path::new(&work_dir));
                    cache.search_list_version = search_list_version;
                    save_resource_cache(std::path::Path::new(&work_dir), &cache);

                    LoopAction::Continue
                }

                GatewayResponse::UserProfileUpdate {
                    user_identity,
                    version,
                } => {
                    tracing::info!(
                        has_profile = user_identity.is_some(),
                        version = version,
                        "Received UserProfileUpdate at runtime — updating identity context"
                    );

                    session_manager.update_user_identity(user_identity);

                    // Persist user_profile_version to resource_cache.json
                    // for next startup's AgentHello diff sync.
                    let mut cache = read_resource_cache(std::path::Path::new(&work_dir));
                    cache.user_profile_version = version;
                    save_resource_cache(std::path::Path::new(&work_dir), &cache);

                    LoopAction::Continue
                }

                GatewayResponse::WorkspaceConfigUpdate { config_json } => {
                    tracing::info!(
                        config_len = config_json.len(),
                        "Received WorkspaceConfigUpdate from Gateway"
                    );

                    // 1. Write config to agent_workspaces.json (atomically)
                    if let Err(e) = crate::tools::workspace_resolver::write_workspace_config(
                        work_dir,
                        &config_json,
                    ) {
                        tracing::error!(
                            error = %e,
                            "Failed to write agent_workspaces.json from WorkspaceConfigUpdate"
                        );
                        return LoopAction::Continue;
                    }

                    // 2. Reload the shared WorkspaceResolver (hot-reload path whitelist)
                    {
                        let mut w = resolver.write().unwrap();
                        *w = crate::tools::workspace_resolver::WorkspaceResolver::reload(work_dir);
                    }

                    // 3. Update default workspace for new sessions from last_active
                    let resolver_guard = resolver.read().unwrap();
                    if let Some(ws_id) = resolver_guard.last_active_workspace_id() {
                        session_manager.set_default_workspace_id(ws_id);
                    }

                    // 4. Refresh context for ALL active sessions (not just current).
                    // Workspace list CRUD: all sessions reconcile lazily when
                    // switched to foreground, but prompt_file changes need to
                    // take effect immediately for the user's active session.
                    session_manager.reconcile_deleted_workspaces(&resolver_guard);
                    // Must release the read lock before the loop below —
                    // update_session_workspace_context re-acquires it internally.
                    drop(resolver_guard);

                    // 5. Push workspace context + prompt_file content to every active
                    // session. This ensures prompt_file changes (e.g. AGENTS.md)
                    // are reflected immediately without requiring a workspace switch.
                    let active_sessions = session_manager.active_sessions();
                    for sid in &active_sessions {
                        session_manager.update_session_workspace_context(sid);
                    }
                    tracing::info!(
                        session_count = active_sessions.len(),
                        "Workspace config applied: file written, resolver reloaded, contexts refreshed"
                    );
                    LoopAction::Continue
                }

                GatewayResponse::SetSessionWorkspace {
                    session_id,
                    workspace_id,
                } => {
                    tracing::info!(
                        session_id = %session_id,
                        workspace_id = %workspace_id,
                        "Received SetSessionWorkspace from Gateway"
                    );

                    // Validate workspace exists or is "__agent_home__"
                    // Use a single lock acquisition to avoid TOCTOU issues.
                    let resolver_guard = resolver.read().unwrap();
                    let is_valid = workspace_id == "__agent_home__"
                        || resolver_guard
                            .allowed_dirs()
                            .iter()
                            .any(|d| d.id == workspace_id);
                    if !is_valid {
                        tracing::warn!(
                            session_id = %session_id,
                            workspace_id = %workspace_id,
                            "SetSessionWorkspace: workspace not in list, setting as pending + fallback"
                        );
                        session_manager
                            .add_pending_workspace(&session_id, &workspace_id);
                        session_manager.set_session_workspace_with_resolver(
                            &session_id,
                            "__agent_home__",
                        );
                    } else {
                        session_manager.set_session_workspace_with_resolver(
                            &session_id,
                            &workspace_id,
                        );
                    }

                    // Format and send per-session workspace context.
                    // Must release the read lock first — update_session_workspace_context
                    // re-acquires it internally.
                    drop(resolver_guard);
                    session_manager.update_session_workspace_context(&session_id);
                    LoopAction::Continue
                }

                GatewayResponse::LogLevelUpdate { log_level } => {
                    tracing::info!(
                        new_level = %log_level,
                        "Received LogLevelUpdate from Gateway"
                    );
                    if let Some(handle) = &log_reload_handle {
                        let new_filter = EnvFilter::new(&log_level);
                        if let Err(e) = handle.reload(new_filter) {
                            tracing::error!(
                                error = %e,
                                "Failed to reload log level"
                            );
                        } else {
                            tracing::info!(
                                level = %log_level,
                                "Log level updated successfully"
                            );
                        }
                    } else {
                        tracing::warn!(
                            level = %log_level,
                            "No reload handle available — cannot update log level dynamically"
                        );
                    }

                    LoopAction::Continue
                }

                GatewayResponse::LogRotate => {
                    tracing::info!("Received LogRotate from Gateway");

                    // 1. Force-rotate to close current file handle and create a new one
                    if let Some(appender) = FILE_APPENDER.get() {
                        appender.force_rotate();
                    }

                    // 2. Delete old log files (handle is now on the new file)
                    let logs_dir = std::path::Path::new(work_dir).join("logs");
                    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
                        let mut deleted = 0u64;
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|ext| ext == "log") {
                                if let Err(e) = std::fs::remove_file(&path) {
                                    tracing::warn!("Failed to delete log file {:?}: {}", path, e);
                                } else {
                                    deleted += 1;
                                }
                            }
                        }

                        tracing::info!("Deleted {} runtime log files", deleted);
                    }

                    LoopAction::Continue
                }

                GatewayResponse::LogFileCountUpdate { log_file_count } => {
                    tracing::info!(
                        count = log_file_count,
                        "Received LogFileCountUpdate from Gateway — enforcing limit"
                    );
                    if let Some(appender) = FILE_APPENDER.get() {
                        let max = if log_file_count > 0 {
                            log_file_count as usize
                        } else {
                            0
                        };
                        appender.set_max_file_count(max);
                        tracing::info!(
                            log_file_count = log_file_count,
                            "Runtime log file count updated dynamically"
                        );
                    }
                    LoopAction::Continue
                }

                GatewayResponse::RuntimeConfigUpdate {
                    max_output_tokens,
                    max_iterations,
                    temperature,
                    system_prompt_override,
                    shell_approval_threshold,
                    mcp_servers,
                    model: _,    // ADR-012: model_switch is a separate action
                    provider: _, // ADR-012: model_switch is a separate action
                    search_config_json,
                    avatar,
                    builtin_avatar,
                    max_sessions,
                    context_window,
                    approval_timeout_secs,
                    builtin_tools_enabled,
                    tool_result_compression_mode, // ADR-032 C4b: plumbed through proto; forwarded below.
                    // ADR-032 C4a: pass the live-edited threshold through to
                    // `RuntimeConfigOverrides` so it (a) updates the in-memory
                    // cache via `merge` and (b) lands in `agent_config.json` via
                    // the persistence block below (lines around 2120). On next
                    // boot, `From<&AgentConfig> for RuntimeConfigOverrides` will
                    // re-derive it into `soft_threshold_chars_override`, which
                    // is what `compress_tool_results` reads.
                    //
                    // **Do not** revert this to `tool_result_soft_threshold_chars: _`
                    // — that pattern was the original bug in commit 75e0349 (for
                    // `tool_result_compression_mode`) and would silently discard
                    // the user's UI input. See `live_edit_*` regression tests.
                    tool_result_soft_threshold_chars,
                } => {
                    tracing::info!(

                        max_output_tokens = ?max_output_tokens,
                        max_iterations = ?max_iterations,
                        temperature = ?temperature,
                        shell_approval_threshold = ?shell_approval_threshold,
                        context_window = ?context_window,

                        mcp_server_count = mcp_servers.as_ref().map(|s| s.len()),
                        "Received RuntimeConfigUpdate from Gateway — applying to current and future sessions"

                    );

                    // Use `apply_runtime_config_override` (not raw `broadcast`)
                    // so the override is also cached on the SessionManager
                    // and replayed to sessions created *after* this push.
                    // Otherwise the untouched `Arc<AgentCore>` template would
                    // silently revert values like `max_iterations` back to
                    // the default (50) for every brand-new session.
                    session_manager.apply_runtime_config_override(
                        &RuntimeConfigOverrides {
                            max_output_tokens,
                            max_iterations,
                            temperature,
                            context_window,
                            system_prompt_override,
                            shell_approval_threshold,
                            approval_timeout_secs,
                            // ADR-032 C4a: pass the live-edited threshold through to
                            // `RuntimeConfigOverrides` so it (a) updates the
                            // in-memory cache via `merge` and (b) lands in
                            // `agent_config.json` via the persistence block
                            // below. On next boot, `From<&AgentConfig> for
                            // RuntimeConfigOverrides` re-derives it into
                            // `soft_threshold_chars_override`, which is what
                            // `compress_tool_results` reads.
                            //
                            // **Do not** revert this to
                            // `tool_result_soft_threshold_chars: None` — that
                            // was the original bug pattern in commit 75e0349
                            // for `tool_result_compression_mode` and silently
                            // discarded the user's UI input. See
                            // `live_edit_*` regression tests.
                            //
                            // ADR-032 C4a: tool_result_keep_recent_n stays
                            // None because it's sourced from
                            // `agent_config.json` at boot (not carried in
                            // `RuntimeConfigUpdate`); see comment near
                            // `tool_result_keep_recent_n` field above.
                            tool_result_keep_recent_n: None,
                            // ADR-032 C4b: pass `tool_result_compression_mode`
                            // through verbatim. The Desktop and the Gateway
                            // DTO own the "what counts as auto / manual /
                            // unset" decision; the runtime is intentionally
                            // dumb here and just forwards the wire value.
                            //
                            // Do **not** normalize `"auto"`, `""`, or any
                            // other variant to `None` here — that would
                            // silently swallow a user's explicit selection.
                            // The only way to "clear" the field is via the
                            // Gateway DTO contract (`null` → `None` →
                            // `apply_to` skips → field absent on disk).
                            tool_result_compression_mode,
                            // proto / grpc gives us `u64`; widen to `usize` for
                            // in-process use. On a 64-bit target (the only
                            // platform we ship for) this is a no-op; on a
                            // 32-bit target a threshold > 4 GiB would truncate,
                            // which is acceptable since `compress_tool_results`
                            // only ever sees message bodies < a few MiB.
                            tool_result_soft_threshold_chars: tool_result_soft_threshold_chars.map(|v| v as usize),
                        },
                    );

                    // ── ADR-029: Apply builtin tools enabled list ──────
                    // When the Gateway pushes a new enabled-tools list,
                    // patch agent_tools.json and broadcast to sessions.
                    if let Some(ref enabled_names) = builtin_tools_enabled {
                        tracing::info!(
                            enabled_names = ?enabled_names,
                            "RuntimeConfigUpdate: received builtin_tools_enabled list"
                        );
                        let work_path = std::path::Path::new(&work_dir);
                        let current = crate::agent_config::load_agent_tools_config(work_path)
                            .unwrap_or_default()
                            .map(|cfg| cfg.tools)
                            .unwrap_or_default();
                        tracing::info!(
                            current_tool_count = current.len(),
                            "RuntimeConfigUpdate: loaded current agent_tools.json for patching"
                        );
                        // Build a patch: every tool in `enabled_names` gets enabled=true;
                        // everything currently in agent_tools.json but not in the list
                        // gets enabled=false.
                        let patch: Vec<crate::agent_config::AgentToolEntry> = current
                            .iter()
                            .map(|entry| {
                                let enabled = enabled_names.iter().any(|n| n == &entry.name);
                                crate::agent_config::AgentToolEntry::new(&entry.name, enabled)
                            })
                            .collect();
                        let updated = crate::agent_config::apply_builtin_tools_patch(&current, &patch);
                        // Log each entry after patching
                        for entry in &updated {
                            tracing::info!(
                                tool = %entry.name,
                                enabled = entry.enabled,
                                "RuntimeConfigUpdate: patched tool entry"
                            );
                        }
                        // Persist
                        if let Err(e) = crate::agent_config::save_agent_tools_config(
                            work_path,
                            &crate::agent_config::AgentToolsConfig {
                                tools: updated.clone(),
                            },
                        ) {
                            tracing::warn!(error = %e, "Failed to persist agent_tools.json after RuntimeConfigUpdate");
                        }
                        // Broadcast to all sessions
                        session_manager.apply_builtin_tools_enabled(&updated);
                        tracing::info!(
                            enabled_count = updated.iter().filter(|e| e.enabled).count(),
                            total = updated.len(),
                            "Applied builtin_tools_enabled from Gateway"
                        );
                    }

                    // Handle MCP server config changes: connect, disconnect, or reconnect.
                    // Gateway pushes catalog MCPs. We must merge catalog + local
                    // before connecting, and only persist the catalog portion.
                    //
                    // Disconnect is done inline (fast) to release old connections
                    // immediately.  Connect is spawned as a background task to
                    // avoid blocking the Gateway message loop for up to 30 seconds
                    // per server.  Results are applied via the mcp_runtime_rx
                    // channel in the select! loop (apply_mcp_connection_result).
                    let mcp_for_persist = mcp_servers.clone();
                    if let Some(catalog_mcp_configs) = mcp_servers {
                        // Merge catalog + local for full MCP connection list
                        let merged = crate::agent_config::load_agent_mcp_config(
                            std::path::Path::new(&work_dir),
                        )
                        .unwrap_or_default()
                        .unwrap_or_default();
                        let full_mcp_configs = AgentMcpConfig {
                            catalog: catalog_mcp_configs.clone(),
                            local: merged.local,
                        }
                        .merged();

                        // Disconnect existing MCP connections inline (fast).
                        session_manager.apply_mcp_servers(vec![]).await;

                        // Spawn background task for connecting new MCP servers.
                        // The result is sent via mcp_runtime_tx and applied
                        // asynchronously in the select! loop.
                        let tx = mcp_runtime_tx.clone();
                        tokio::spawn(async move {
                            let (registry, failures) =
                                acowork_mcp::client::McpRegistry::connect_all(&full_mcp_configs)
                                    .await
                                    .expect("connect_all is non-fatal and should never fail");
                            let registry = std::sync::Arc::new(registry);
                            let mut wrappers = Vec::new();
                            let mut specs = Vec::new();
                            for prefixed_name in registry.tool_names() {
                                if let Some(def) = registry.get_tool_def(&prefixed_name) {
                                    let wrapper = acowork_mcp::wrapper::McpToolWrapper::new(
                                        prefixed_name.clone(),
                                        def,
                                        registry.clone(),
                                    );
                                    use acowork_core::tools::traits::Tool;
                                    let tool_spec = wrapper.spec();
                                    let serialized =
                                        serde_json::to_value(&tool_spec).unwrap_or_default();
                                    specs.push((tool_spec.name.clone(), serialized));
                                    wrappers.push(wrapper);
                                }
                            }
                            let _ = tx.send((registry, wrappers, specs, failures)).await;
                        });
                    }

                    // Handle per-agent search config persistence.
                    // When `search_config_json` is Some, parse and save to agent_search.json.
                    // When None, preserve existing (no change).
                    if let Some(ref search_json) = search_config_json {
                        if search_json.is_empty() {
                            // Remove agent_search.json when empty config is pushed
                            let search_path = std::path::Path::new(&work_dir)
                                .join("config")
                                .join("agent_search.json");
                            if search_path.exists() {
                                let _ = std::fs::remove_file(&search_path);
                                tracing::info!("Removed agent_search.json (empty config)");
                            }
                        } else {
                            match serde_json::from_str::<acowork_core::protocol::AgentSearchConfig>(
                                search_json,
                            ) {
                                Ok(search_cfg) => {
                                    let _ = crate::agent_config::save_agent_search_config(
                                        std::path::Path::new(&work_dir),
                                        &search_cfg,
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "Failed to parse search_config_json in RuntimeConfigUpdate"
                                    );
                                }
                            }
                        }
                    }

                    // Persist per-agent config to workspace/config/agent_config.json.
                    // This consolidates all overrides into a single file owned by Runtime,
                    // replacing the former Gateway-side data/agent_configs/{agent_id}.json.
                    //
                    // Read-modify-write: load the existing config so unknown fields
                    // are preserved during Runtime writes.
                    {
                        let overrides = &session_manager.runtime_overrides;
                        let mut agent_cfg = crate::agent_config::load_agent_config(
                            std::path::Path::new(&work_dir),
                        )
                        .unwrap_or_default()
                        .unwrap_or_default();
                        // Apply the runtime override cache onto AgentConfig.
                        // `apply_to` is the single source of truth for the
                        // RuntimeConfigOverrides → AgentConfig mapping and only
                        // writes fields whose override is `Some(...)`, so partial
                        // pushes do not clobber unrelated disk values.
                        //
                        // This is what fixes the bug where Desktop edits of
                        // `tool_result_compression_mode` never reached
                        // `agent_config.json` (the field was previously dropped
                        // by the live-edit path and not present in the manual
                        // read-modify-write below — see ADR-032 C4b regression).
                        overrides.apply_to(&mut agent_cfg);
                        // ADR-017: Apply avatar config from RuntimeConfigUpdate.
                        // Some("path") = set, Some("") = clear, None = don't change.
                        if let Some(ref av) = avatar {
                            agent_cfg.avatar = if av.is_empty() { None } else { Some(av.clone()) };
                        }
                        if let Some(ref ba) = builtin_avatar {
                            agent_cfg.builtin_avatar = if ba.is_empty() { None } else { Some(ba.clone()) };
                        }
                        // ADR-024: Apply max_sessions from RuntimeConfigUpdate.
                        if let Some(ms) = max_sessions {
                            agent_cfg.max_sessions = Some(ms);
                        }
                        let _ = crate::agent_config::save_agent_config(
                            std::path::Path::new(&work_dir),
                            &agent_cfg,
                        );
                        // Persist only catalog portion to agent_mcp.json,
                        // preserving local entries (agent-installed MCPs).
                        if let Some(ref catalog_mcp_servers) = mcp_for_persist {
                            let _ = crate::agent_config::save_agent_mcp_config_catalog(
                                std::path::Path::new(&work_dir),
                                catalog_mcp_servers,
                            );
                        }
                    }

                    LoopAction::Continue
                }

                GatewayResponse::EnableDebugMode { debug_port } => {
                    tracing::info!(
                        debug_port = debug_port,
                        "Received EnableDebugMode from Gateway — starting debug server"
                    );

                    // 1. Fire urgent_stop to ALL sessions — EnableDebugMode
                    //    requires all sessions to stop so they restart with
                    //    debug capabilities injected.
                    // ADR-033: gRPC removed — fire_urgent_stop_all is no-op.

                    // 2. Start the DebugProtocolServer and store handles so that
                    //    sessions created *after* this call inherit debug mode.
                    session_manager.enable_debug_mode(debug_port).await;

                    LoopAction::Continue
                }

                // ── ADR-030 C3: Generic sidecar endpoint update ────────
                //
                // Gateway pushes `SidecarEndpointUpdate` whenever a
                // managed sidecar's reachable endpoint changes. We
                // route by `sidecar`:
                //
                // * `LspRelay` — register or unregister the `codebase`
                //   builtin tool so the LLM dispatch list reflects the
                //   current LSP relay availability in real time.
                // * `Embed`    — delegate to `handle_embedding_config_update`
                //   (payload is identical to the removed `EmbeddingConfigUpdate`
                //   variant, now delivered via the canonical channel).
                // * `Unspecified` — unknown / reserved; log and ignore.
                GatewayResponse::SidecarEndpointUpdate {
                    sidecar,
                    endpoint,
                    spec_json,
                } => {
                    use acowork_core::protocol::SidecarKind;
                    tracing::info!(
                        sidecar = %sidecar.as_str(),
                        endpoint = %endpoint,
                        spec_len = spec_json.len(),
                        "Received SidecarEndpointUpdate from Gateway"
                    );
                    match sidecar {
                        SidecarKind::LspRelay => {
                            if endpoint.is_empty() {
                                tracing::info!(
                                    "SidecarEndpointUpdate(LspRelay): endpoint empty, unregistering codebase"
                                );
                                session_manager.unregister_dynamic_tool("codebase");
                                // Persist removal to agent_tools.json so the
                                // frontend tool panel stops showing codebase.
                                crate::agent_config::remove_tool_from_config(
                                    std::path::Path::new(&work_dir),
                                    "codebase",
                                );
                            } else {
                                tracing::info!(
                                    endpoint = %endpoint,
                                    "SidecarEndpointUpdate(LspRelay): endpoint ready, registering codebase"
                                );
                                let tool: Arc<dyn acowork_core::tools::traits::Tool> =
                                    Arc::new(crate::tools::builtin::codebase::CodebaseTool::new(
                                        endpoint.clone(),
                                    ));
                                // enabled=true on register; if the user has
                                // codebase disabled in agent_tools.json the
                                // entry will still be added (with enabled=true)
                                // and the per-session `enabled` flag is the
                                // gateway-driven source of truth. The
                                // codebase tool being added here is the
                                // *availability* signal; the user's enable
                                // preference is reflected via the builtin
                                // tools enabled list and filtered by
                                // `rebuild_all_tools` accordingly.
                                session_manager.register_dynamic_tool(
                                    tool,
                                    resolver.clone(),
                                    60,
                                    true,
                                );
                                // Persist addition to agent_tools.json so the
                                // frontend tool panel shows codebase. If the
                                // entry already exists, the user's enabled/
                                // disabled preference is preserved.
                                crate::agent_config::ensure_tool_in_config(
                                    std::path::Path::new(&work_dir),
                                    "codebase",
                                    true,
                                );
                            }
                        }
                        SidecarKind::Embed => {
                            if endpoint.is_empty() {
                                // Embed sidecar unavailable: clear the ONNX
                                // embedding provider on all sessions so
                                // embedding operations degrade gracefully
                                // (ADR-030 review ISSUE-2 fix).
                                tracing::info!(
                                    "SidecarEndpointUpdate(Embed) with empty endpoint; \
                                     clearing embedding provider on all sessions"
                                );
                                session_manager.clear_embedding_config();
                            } else {
                                // spec_json shape (from C2 build_embed_sidecar_payload):
                                //   {"model_id":"...","dimension":N}
                                #[derive(serde::Deserialize)]
                                struct EmbedSpec {
                                    model_id: String,
                                    dimension: usize,
                                }
                                match serde_json::from_str::<EmbedSpec>(&spec_json) {
                                    Ok(spec) => {
                                        session_manager.handle_embedding_config_update(
                                            endpoint.clone(),
                                            spec.model_id,
                                            spec.dimension,
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            spec_json = %spec_json,
                                            "Failed to parse Embed spec_json in SidecarEndpointUpdate"
                                        );
                                    }
                                }
                            }
                        }
                        SidecarKind::Unspecified => {
                            tracing::warn!(
                                "Received SidecarEndpointUpdate with Unspecified kind; ignoring"
                            );
                        }
                    }
                    LoopAction::Continue
                }

                GatewayResponse::MigrationStart {
                    request_id,
                    embed_endpoint,
                    embed_model_id,
                    embed_dimension,
                } => {
                    tracing::info!(
                        request_id = %request_id,
                        endpoint = %embed_endpoint,
                        model_id = %embed_model_id,
                        dimension = embed_dimension,
                        "Received MigrationStart from Gateway — starting embedding dimension migration"
                    );

                    // Stop all sessions to prevent concurrent memory writes
                    // during migration (the user already acknowledged this).
                    // ADR-033: gRPC removed — fire_urgent_stop_all is no-op.

                    let store = session_manager.core().memory_store().cloned();
                    let outbound = grpc_client.outbound_ctrl_sender();
                    let req_id = request_id.clone();
                    let endpoint = embed_endpoint.clone();
                    let model_id = embed_model_id.clone();
                    let dim = embed_dimension;
                    let timeouts = timeouts.clone();

                    if let Some(store) = store {
                        let old_dim = store.embedding_dim();

                        tokio::spawn(async move {
                            // Build the new embedding provider for re-embedding.
                            let migration_provider = crate::embedding::remote::RemoteEmbeddingProvider::with_config_and_timeouts(
                                &endpoint,
                                None,
                                &model_id,
                                dim,
                                &timeouts,
                            );
                            let migration_provider = std::sync::Arc::new(migration_provider)
                                as std::sync::Arc<dyn crate::embedding::EmbeddingProvider>;

                            // Use spawn_blocking so the sync migration (with
                            // handle.block_on for async embed calls) doesn't
                            // starve the tokio worker pool.
                            let result = tokio::task::spawn_blocking({
                                let outbound = outbound.clone();
                                let req_id = req_id.clone();
                                let store = store.clone();
                                let provider = migration_provider.clone();

                                move || {
                                    let handle = tokio::runtime::Handle::current();

                                    // Progress callback: sends a StreamChunk to
                                    // Gateway so the frontend can show progress.
                                    let progress_cb = |processed: u64, total: u64| {
                                        let params = serde_json::json!({
                                            "request_id": req_id,
                                            "processed": processed,
                                            "total": total,
                                            "phase": "reembed",
                                        });
                                        let msg = acowork_core::proto::ClientMessage {
                                            request_id: 0,
                                            payload: Some(
                                                acowork_core::proto::client_message::Payload::StreamChunk(
                                                    acowork_core::proto::StreamChunk {
                                                        target: "http-api".to_string(),
                                                        action: "embedding_migration_progress".to_string(),
                                                        params_json: params.to_string(),
                                                    },
                                                ),
                                            ),
                                        };
                                        let _ = outbound.try_send(msg);
                                    };

                                    // Embed closure: bridges async embed into sync.
                                    let provider_for_fn = provider.clone();
                                    let embed_fn = move |text: &str| -> Option<Vec<f32>> {
                                        let text_owned = text.to_string();
                                        match handle.block_on(provider_for_fn.embed(&text_owned)) {
                                            Ok(vec) => Some(vec),
                                            Err(e) => {
                                                tracing::warn!(
                                                    error = %e,
                                                    "Re-embedding failed during migration"
                                                );
                                                None
                                            }
                                        }
                                    };

                                    store.migrate_embedding_dimension_with_progress(
                                        embed_fn,
                                        dim,
                                        Some(&progress_cb),
                                    )
                                }
                            })
                            .await;

                            // Report completion or failure back to Gateway.
                            match result {
                                Ok(Ok(stats)) => {
                                    tracing::info!(
                                        request_id = %req_id,
                                        old_dim,
                                        new_dim = dim,
                                        rebuilt = stats.rebuilt,
                                        skipped = stats.skipped_no_embedding + stats.skipped_no_content,
                                        errors = stats.errors,
                                        "Embedding dimension migration complete"
                                    );

                                    // Force a WAL checkpoint + full section flush to the .grafeo
                                    // container file so that embeddings survive a process restart
                                    // even when the periodic checkpoint timer (300s) hasn't fired yet.
                                    if let Err(e) = store.db().wal_checkpoint() {
                                        tracing::error!(
                                            request_id = %req_id,
                                            error = %e,
                                            "Failed to checkpoint Grafeo after migration — embeddings \
                                             may be lost on restart (they will only exist in WAL)"
                                        );
                                    } else {
                                        tracing::info!(
                                            request_id = %req_id,
                                            "Grafeo checkpoint flushed after migration"
                                        );
                                    }

                                    // Notify Gateway of completion via IntentSend.
                                    let params = serde_json::json!({
                                        "request_id": req_id,
                                        "success": true,
                                        "old_dim": old_dim,
                                        "new_dim": dim,
                                        "rebuilt": stats.rebuilt,
                                        "skipped": stats.skipped_no_embedding + stats.skipped_no_content,
                                        "errors": stats.errors,
                                    });
                                    let msg = acowork_core::proto::ClientMessage {
                                        request_id: 0,
                                        payload: Some(
                                            acowork_core::proto::client_message::Payload::IntentSend(
                                                acowork_core::proto::IntentSendRequest {
                                                    target: "gateway".to_string(),
                                                    action: "migration_complete".to_string(),
                                                    params_json: params.to_string(),
                                                    r#async: false,
                                                },
                                            ),
                                        ),
                                    };
                                    let _ = outbound.send(msg).await;
                                }
                                Ok(Err(e)) => {
                                    tracing::error!(
                                        request_id = %req_id,
                                        error = %e,
                                        "Embedding dimension migration failed"
                                    );
                                    let params = serde_json::json!({
                                        "request_id": req_id,
                                        "success": false,
                                        "error": format!("{}", e),
                                    });
                                    let msg = acowork_core::proto::ClientMessage {
                                        request_id: 0,
                                        payload: Some(
                                            acowork_core::proto::client_message::Payload::IntentSend(
                                                acowork_core::proto::IntentSendRequest {
                                                    target: "gateway".to_string(),
                                                    action: "migration_complete".to_string(),
                                                    params_json: params.to_string(),
                                                    r#async: false,
                                                },
                                            ),
                                        ),
                                    };
                                    let _ = outbound.send(msg).await;
                                }
                                Err(join_err) => {
                                    tracing::error!(
                                        request_id = %req_id,
                                        error = %join_err,
                                        "Migration task panicked"
                                    );
                                }
                            }
                        });
                    } else {
                        tracing::warn!(
                            "MigrationStart received but no memory store available — skipping migration"
                        );
                    }

                    LoopAction::Continue
                }

                _ => {
                    tracing::debug!("Ignoring non-IntentReceived Gateway message");
                    LoopAction::Continue
                }
            }
        }

        Ok(None) => {
            tracing::info!("Gateway connection closed, attempting reconnect...");

            // Try to reconnect with exponential backoff
            match try_reconnect_gateway(agent_id_for_reconnect, version_for_reconnect, grpc_client)
                .await
            {
                Ok(()) => {
                    tracing::info!("Reconnected to Gateway successfully");
                    LoopAction::Continue
                }

                Err(e) => {
                    tracing::error!("Failed to reconnect to Gateway: {}", e);
                    LoopAction::Break
                }
            }
        }

        Err(e) => {
            tracing::error!("Gateway recv error: {}", e);

            // Don't break on transient errors — try to continue
            tokio::time::sleep(std::time::Duration::from_millis(
                100,
            ))
            .await;
            LoopAction::Continue
        }
    }
}

// ── Memory query handler ────────────────────────────────────────────────────
//
// ADR-033: Memory queries are now served by the Runtime's localhost HTTP
// server (reverse-proxied by the Gateway).  The gRPC path is retained for
// compatibility but is implemented as a thin wire-format adapter over the
// shared [`crate::http::memory_query`] module — the HTTP and gRPC paths
// MUST stay in sync, and that module is the single source of truth.

/// Handle a memory API query from Gateway (via gRPC, not IntentReceived).
///
/// Gateway HTTP handlers proxy memory requests to the Runtime through the
/// gRPC bidirectional stream using request_id correlation. This handler
/// calls GrafeoStore methods and sends the proto response back.
/// Spawned as a tokio task to handle a memory query without blocking the main
/// select! loop. Takes owned data (Arc, Sender) so it can run independently.
#[allow(dead_code)]
async fn spawn_memory_query_handler(
    memory_store: Option<Arc<acowork_grafeo::grafeo::GrafeoStore>>,
    embed_provider_dim: u64,
    outbound: tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    request_id: u64,
    payload: acowork_core::proto::server_message::Payload,
) {
    use acowork_core::proto;
    use acowork_core::proto::server_message::Payload as ServerPayload;
    use crate::http::memory_query::ListNodesParams;
    tracing::info!(
        request_id,
        payload_type = ?std::mem::discriminant(&payload),
        memory_store = memory_store.is_some(),
        embed_provider_dim,
        "Memory query handler spawned"
    );
    let response_payload = match payload {
        ServerPayload::MemoryNodesQuery(q) => {
            let params = ListNodesParams {
                page: q.page,
                size: q.size,
                node_type: q.r#type,
                keyword: q.keyword,
                time_range: q.time_range,
            };
            handle_memory_nodes_query(memory_store.as_ref(), params)
        }
        ServerPayload::MemoryStatsQuery(_) => {
            handle_memory_stats_query(memory_store.as_ref(), embed_provider_dim)
        }
        ServerPayload::MemoryDeleteQuery(q) => {
            handle_memory_delete_query(memory_store.as_ref(), q.node_id)
        }
        ServerPayload::MemoryConsolidateQuery(q) => {
            handle_memory_consolidate_query(memory_store.as_ref(), q.force, q.retention_days)
        }

        _ => {
            tracing::warn!("Unexpected payload in memory query handler");
            return;
        }
    };
    let client_msg = proto::ClientMessage {
        request_id,
        payload: Some(response_payload),
    };
    if outbound.send(client_msg).await.is_err() {
        tracing::warn!(
            request_id,
            "Failed to send memory query response to Gateway"
        );
    }
}

/// Handle MemoryNodesQuery — list nodes with pagination, filtering, search.
///
/// Thin adapter over [`memory_query::list_nodes`]. The shared module owns
/// the scan logic (label iteration, filters, time-range parsing, the P0
/// unfiltered-scan guard); this function just maps the intermediate
/// `MemoryNodeRecord` slice back to the proto wire format used by the
/// legacy gRPC path.
///
/// HTTP handlers in [`crate::http::server`] call the same module and
/// serialize the same records into JSON — keeps both paths consistent.
#[allow(dead_code)]
fn handle_memory_nodes_query(
    memory_store: Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>>,
    params: crate::http::memory_query::ListNodesParams,
) -> acowork_core::proto::client_message::Payload {
    use acowork_core::proto;
    use acowork_core::proto::client_message::Payload as ClientPayload;
    let out = crate::http::memory_query::list_nodes(memory_store, params);
    let nodes: Vec<proto::MemoryNodeEntry> = out
        .nodes
        .into_iter()
        .map(|n| proto::MemoryNodeEntry {
            node_id: n.node_id,
            node_type: n.node_type,
            content: n.content,
            confidence: n.confidence,
            decay_score: n.decay_score,
            created_at: n.created_at,
            last_accessed_at: n.last_accessed_at,
            access_count: n.access_count,
            status: n.status,
        })
        .collect();
    ClientPayload::MemoryNodesResult(proto::MemoryNodesResult {
        total: out.total,
        page: out.page,
        size: out.size,
        nodes,
    })
}

/// Handle MemoryStatsQuery — get memory statistics.
///
/// Thin adapter over [`memory_query::get_stats`].
#[allow(dead_code)]
fn handle_memory_stats_query(
    memory_store: Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>>,
    embed_provider_dim: u64,
) -> acowork_core::proto::client_message::Payload {
    use acowork_core::proto;
    use acowork_core::proto::client_message::Payload as ClientPayload;
    let out = crate::http::memory_query::get_stats(memory_store, embed_provider_dim);
    ClientPayload::MemoryStatsResult(proto::MemoryStatsResult {
        total_nodes: out.total_nodes,
        storage_bytes: out.storage_bytes,
        by_type: out.by_type,
        by_status: out.by_status,
        avg_decay_score: out.avg_decay_score,
        index_health: out.index_health,
        stored_dim: out.stored_dim,
        nodes_with_embedding: out.nodes_with_embedding,
        model_dim: out.model_dim,
    })
}

/// Handle MemoryDeleteQuery — delete a memory node by ID.
///
/// Thin adapter over [`memory_query::delete_node`].
#[allow(dead_code)]
fn handle_memory_delete_query(
    memory_store: Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>>,
    node_id: u64,
) -> acowork_core::proto::client_message::Payload {
    use acowork_core::proto;
    use acowork_core::proto::client_message::Payload as ClientPayload;
    let out = crate::http::memory_query::delete_node(memory_store, node_id);
    ClientPayload::MemoryDeleteResult(proto::MemoryDeleteResult {
        node_id: out.node_id,
        deleted: out.deleted,
        message: out.message,
    })
}

/// Handle MemoryConsolidateQuery — trigger memory consolidation.
///
/// Thin adapter over [`memory_query::trigger_consolidate`].
#[allow(dead_code)]
fn handle_memory_consolidate_query(
    memory_store: Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>>,
    force: bool,
    retention_days: u32,
) -> acowork_core::proto::client_message::Payload {
    use acowork_core::proto;
    use acowork_core::proto::client_message::Payload as ClientPayload;
    let out = crate::http::memory_query::trigger_consolidate(memory_store, force, retention_days);
    ClientPayload::MemoryConsolidateResult(proto::MemoryConsolidateResult {
        started: out.started,
        duration_ms: out.duration_ms,
        episodes_consolidated: out.episodes_consolidated,
        knowledge_nodes_generated: out.knowledge_nodes_generated,
        message: out.message,
    })
}

// ── S1.14: Session query handlers ─────────────────────────────────────────────

/// Handle "list_sessions" action from Gateway (S1.14)
///
/// Scans the conversations directory for JSONL session files,
/// converts the results to SessionInfoDto, and sends them back
/// to Gateway via IntentSend with action "session_response".
///
/// ADR-014: Merges live session status from SessionManager into
/// the DTOs, so the frontend gets real-time status via Pull path.
#[allow(dead_code)]
async fn handle_list_sessions(
    work_dir: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
    session_manager: &crate::agent::session::session_manager::SessionManager,
) {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let page = params
        .get("page")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let size = params
        .get("size")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let conversations_dir = std::path::PathBuf::from(work_dir).join("conversations");
    let handle = crate::conversation::scan_sessions_async(conversations_dir, page, size);
    let (sessions, total_count, agent_totals) = match handle.await {
        Ok(result) => result,

        Err(e) => {
            tracing::error!("Failed to scan sessions: {}", e);
            (Vec::new(), 0, (0, 0))
        }
    };

    // ADR-028: fold the on-disk scan totals into the AgentCore counters via
    // atomic-max merge. This rebuilds the historical baseline after a
    // process restart and reconciles any out-of-band writers that updated
    // `SessionTokens` behind us. After the merge we read the live values
    // back so the response payload always reflects the post-merge state
    // (which may exceed the scanned value if fresh LLM calls raced the scan).
    session_manager.core().merge_token_totals((
        Some(agent_totals.0),
        Some(agent_totals.1),
    ));
    let (agent_total_input_tokens, agent_total_output_tokens) =
        session_manager.core().agent_token_totals();

    let page_size = size.unwrap_or(20) as usize;
    let total_pages = if total_count == 0 {
        1
    } else {
        total_count.div_ceil(page_size)
    };

    // ADR-014: Collect live session statuses from SessionManager
    let live_statuses: std::collections::HashMap<
        String,
        crate::agent::session_state::SessionStatus,
    > = session_manager.session_statuses().into_iter().collect();
    let session_dtos: Vec<acowork_core::protocol::SessionInfoDto> = sessions
        .into_iter()
        .map(|s| {
            let status = live_statuses.get(&s.session_id).map(|st| {
                // Convert SessionStatus → SessionStatusDto
                match st {
                    crate::agent::session_state::SessionStatus::Idle => {
                        acowork_core::protocol::SessionStatusDto::Idle
                    }

                    crate::agent::session_state::SessionStatus::Streaming { message_id } => {
                        acowork_core::protocol::SessionStatusDto::Streaming {
                            message_id: message_id.clone(),
                        }
                    }

                    crate::agent::session_state::SessionStatus::WaitingApproval { request_id } => {
                        acowork_core::protocol::SessionStatusDto::WaitingApproval {
                            request_id: request_id.clone(),
                        }
                    }

                    crate::agent::session_state::SessionStatus::Paused {
                        iteration,
                        max_iterations,
                        retry_info,
                    } => acowork_core::protocol::SessionStatusDto::Paused {
                        iteration: *iteration,
                        max_iterations: *max_iterations,
                        retry_info: retry_info.as_ref().map(|ri| acowork_core::protocol::RetryPauseInfoDto {
                            wait_ms: ri.wait_ms,
                            attempt: ri.attempt,
                            max_attempts: ri.max_attempts,
                            provider: ri.provider.clone(),
                        }),
                    },
                }
            });
            let ws_id = session_manager.session_workspace_id(&s.session_id);
            let workspace_id = if ws_id == "__agent_home__" {
                None
            } else {
                Some(ws_id)
            };
            acowork_core::protocol::SessionInfoDto {
                session_id: s.session_id,
                created_at: s.created_at,
                message_count: s.message_count,
                title: s.title,
                corrupted: s.corrupted,
                status,
                workspace_id,
                model: s.model,
                provider: s.provider,
            }
        })
        .collect();
    let data = serde_json::json!({
        "sessions": session_dtos,
        "total_count": total_count,
        "total_pages": total_pages,
        // ADR-028: agent-scoped cumulative token totals, sourced from
        // AgentCore after atomic-max merge with the on-disk scan. Acts
        // as a fallback data source for the Desktop App's Results Panel
        // before any per-session cumulative figure is available.
        "agent_total_input_tokens": agent_total_input_tokens,
        "agent_total_output_tokens": agent_total_output_tokens,
    });
    send_session_response(grpc_client, &request_id, data).await;
}

/// Handle "get_session_messages" action from Gateway (S1.14)
///
/// Reads paginated messages from the specified session's JSONL file
/// and sends them back to Gateway via IntentSend.
#[allow(dead_code)]
/// ADR-035 D9.2: truncate tool_result content to first 5 lines for display.
/// Full content stays in JSONL (LLM context). No exception — all paths
/// (MQTT push, HTTP first-page, scroll-back, reconnect realign) use this.
pub(crate) fn truncate_tool_result_for_display(role: &str, content: &str) -> String {
    if role != "tool_result" {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 5 {
        return content.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
}

async fn handle_get_session_messages(
    work_dir: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
    session_manager: &SessionManager,
) {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_id = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
    // ADR-035: `offset` and `limit` are both measured in raw entries
    // (one JSONL line each).  Display-group collapsing is a frontend
    // UI abstraction; the CLI protocol never reasons about groups.
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32;

    // ADR-035 Phase 3: incremental polling parameters removed.
    // Frontend no longer sends `incremental`, `line_number`, or
    // `line_char_offset`. Only offset-based pagination remains.

    if session_id.is_empty() {
        let data = serde_json::json!({
            "error": "session_id is required",
        });
        send_session_response(grpc_client, &request_id, data).await;
        return;
    }

    let file_path = std::path::PathBuf::from(work_dir)
        .join("conversations")
        .join(format!("{}.jsonl", session_id));
    if !file_path.exists() {
        let data = serde_json::json!({
            "error": format!("Session {} not found", session_id),
        });
        send_session_response(grpc_client, &request_id, data).await;
        return;
    }

    // ADR-035 Phase 3: only offset-based pagination remains.  The
    // `incremental` cursor path and `line_number` coordinate path are
    // removed — streaming data arrives exclusively via MQTT push.
    match crate::conversation::read_messages_paginated(&file_path, offset, limit) {
        Ok(paginated) => {
            let message_dtos: Vec<acowork_core::protocol::ConversationEntryDto> = paginated
                .messages
                .into_iter()
                .map(|m| acowork_core::protocol::ConversationEntryDto {
                    id: m.id,
                    ts: m.ts,
                    content: truncate_tool_result_for_display(&m.role, &m.content),
                    role: m.role,
                    metadata: m.metadata,
                    kind: m.kind,
                })
                .collect();
            let total_lines = session_manager.committed_lines_for(&session_id);
            // ADR-025: Reset delivery cursor on non-incremental full load —
            // all existing lines are now "delivered" via pagination.
            session_manager.reset_delivery_cursor(&session_id, total_lines);
            let data = serde_json::json!({
                "messages": message_dtos,
                "offset": paginated.offset,
                "limit": paginated.limit,
                "total": paginated.total,
                "total_lines": total_lines,
            });
            send_session_response(grpc_client, &request_id, data).await;
        }

        Err(e) => {
            tracing::error!("Failed to read session messages: {}", e);
            let data = serde_json::json!({
                "error": format!("Failed to read messages: {}", e),
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
    }
}

/// Send a session response back to Gateway via IntentSend (S1.14)
///
/// Wraps the response data with the request_id and sends it
/// as an IntentSend with action "session_response" targeting "http-api".
/// Relay a StreamChunk message to Gateway (used by chunk relay task).
///
/// StreamChunk is the lightweight path for real-time streaming deltas
/// (agent_reasoning_started, agent_chunk). These go directly to the
/// WebSocket bridge without requiring an IntentSend round-trip.
#[allow(dead_code)]
pub(crate) async fn relay_stream_chunk(
    outbound_tx: &tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    action: &str,
    params: &serde_json::Value,
) {
    let msg = acowork_core::proto::ClientMessage {
        request_id: 0,

        payload: Some(acowork_core::proto::client_message::Payload::StreamChunk(
            acowork_core::proto::StreamChunk {
                target: "http-ws".to_string(),
                action: action.to_string(),
                params_json: params.to_string(),
            },
        )),
    };
    if outbound_tx.send(msg).await.is_err() {
        tracing::debug!(
            "{} relay send failed — main connection may be closed",
            action
        );
    }
}

/// Non-blocking variant of [`relay_stream_chunk`] for high-frequency data
/// events (Delta, ReasoningDelta).
///
/// Uses `try_send` instead of `send().await`: if the outbound channel is
/// full, the event is silently dropped instead of blocking the relay task.
/// This is acceptable for data events — dropping a single delta only causes
/// a minor display glitch, whereas blocking would stall control events
/// (Stopped, SessionStateChanged) that share the relay loop.
#[allow(dead_code)]
pub(crate) fn try_relay_stream_chunk(
    outbound_tx: &tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    action: &str,
    params: &serde_json::Value,
) {
    let msg = acowork_core::proto::ClientMessage {
        request_id: 0,

        payload: Some(acowork_core::proto::client_message::Payload::StreamChunk(
            acowork_core::proto::StreamChunk {
                target: "http-ws".to_string(),
                action: action.to_string(),
                params_json: params.to_string(),
            },
        )),
    };
    if outbound_tx.try_send(msg).is_err() {
        tracing::trace!(
            "{} relay try_send dropped (outbound full) — acceptable for data events",
            action
        );
    }
}

/// Relay an IntentSend message to Gateway (used by chunk relay task).
///
/// IntentSend is the full-round-trip path for discrete events
/// (tool_call, tool_result, agent_response, etc.) that may require
/// ack/nack handling downstream.
pub(crate) async fn relay_intent(
    outbound_tx: &tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    action: &str,
    params: &serde_json::Value,
) {
    let target = if action == "tool_approval_needed" {
        "http-api"
    } else {
        "http-ws"
    };
    let msg = acowork_core::proto::ClientMessage {
        request_id: 0,

        payload: Some(acowork_core::proto::client_message::Payload::IntentSend(
            acowork_core::proto::IntentSendRequest {
                target: target.to_string(),
                action: action.to_string(),
                params_json: params.to_string(),
                r#async: false,
            },
        )),
    };
    if outbound_tx.send(msg).await.is_err() {
        tracing::debug!(
            "{} relay send failed — main connection may be closed",
            action
        );
    }
}

#[allow(dead_code)]
async fn send_session_response(
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    request_id: &str,
    data: serde_json::Value,
) {
    let params = serde_json::json!({
         "request_id": request_id,
         "data": data,
    });
    if let Err(e) = grpc_client
        .send_intent("http-api", "session_response", params, true)
        .await
    {
        tracing::error!(
            request_id = %request_id,
            error = %e,
            "Failed to send session response to Gateway"
        );
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_cli_gateway_socket_arg() {
        let cli = Cli::parse_from([
            "acowork-runtime",
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
        assert_eq!(
            cli.gateway_socket,
            Some("unix:///tmp/gateway.sock".to_string())
        );
    }

    #[tokio::test]
    async fn test_gateway_client_connection_failure_graceful() {
        // Use a non-existent socket path to force connection failure.

        // Use connect_with_timeout directly to avoid the default 300s

        // gRPC connect retry budget.
        let result = crate::grpc::client::GatewayGrpcClient::connect_with_timeout(
            "unix:///nonexistent/socket/path.sock",
            2, // 2-second max elapsed time — enough to try a few times
            256, // default outbound ctrl capacity
        )
        .await;
        assert!(
            result.is_err(),
            "Should gracefully return error on connection failure"
        );
    }
}

// ── Skill mode resolution (manifest default + user override) ────────────

/// User runtime override for skill configuration.
///
/// Stored at `{work_dir}/.agent_skills.json` and takes precedence over
/// the manifest's default `[skills]` configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct AgentSkillsOverride {
    /// Whether to use progressive skill injection mode.
    #[serde(default)]
    progressive: Option<bool>,
}

/// Resolve the effective skill mode by merging manifest default with user override.
///
/// Priority: `{work_dir}/.agent_skills.json` > manifest `[skills]` default.
pub(crate) fn resolve_skill_mode(
    manifest: &acowork_core::AgentManifest,

    work_dir: &str,
) -> acowork_core::SkillMode {
    let default_progressive = manifest.skills.progressive;

    // Check for user override in workspace
    let override_path = std::path::Path::new(work_dir).join(".agent_skills.json");
    if override_path.exists() {
        match std::fs::read_to_string(&override_path) {
            Ok(content) => match serde_json::from_str::<AgentSkillsOverride>(&content) {
                Ok(override_config) => {
                    if let Some(progressive) = override_config.progressive {
                        tracing::info!(
                            progressive = %progressive,
                            manifest_default = %default_progressive,
                            "Skill mode overridden by .agent_skills.json"
                        );
                        return if progressive {
                            acowork_core::SkillMode::Progressive
                        } else {
                            acowork_core::SkillMode::Manual
                        };
                    }
                }

                Err(e) => {
                    tracing::warn!(
                        path = %override_path.display(),
                        error = %e,
                        "Failed to parse .agent_skills.json, using manifest default"
                    );
                }
            },

            Err(e) => {
                tracing::warn!(
                    path = %override_path.display(),
                    error = %e,
                    "Failed to read .agent_skills.json, using manifest default"
                );
            }
        }
    }

    manifest.skill_mode()
}

/// Attempt to reconnect to the Gateway via gRPC with exponential backoff.
///
/// Called when the gRPC connection drops (Gateway restart, network issue, etc.).
/// Returns Ok(()) if reconnection succeeds, Err if all attempts fail.
#[allow(dead_code)]
async fn try_reconnect_gateway(
    agent_id: &str,

    version: &str,

    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
) -> Result<()> {
    match grpc_client
        .reconnect_and_reregister(agent_id, version)
        .await
    {
        Ok(()) => {
            tracing::info!("Reconnected to Gateway gRPC successfully");
            Ok(())
        }

        Err(e) => {
            tracing::error!("Failed to reconnect to Gateway gRPC: {}", e);
            Err(crate::error::RuntimeError::Ipc(format!(
                "gRPC reconnect failed: {}",
                e
            )))
        }
    }
}
