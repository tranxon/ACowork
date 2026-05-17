//! CLI definitions for Agent Runtime

use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};

use crate::agent::agent_core::AgentCore;

use crate::agent::inbound::InboundMessage;
use crate::agent::session::{SessionManager, SessionManagerConfig, SessionMessage};
use crate::config::RuntimeConfig;
use crate::error::Result;
use std::sync::Arc;
use tokio::sync::Notify;

/// Type alias for the reload handle used to dynamically change log level.
pub type LogReloadHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Retry interval when Gateway recv encounters a transient error
const GATEWAY_RECV_RETRY_INTERVAL_MS: u64 = 100;

/// Global reference to the SizeRollingFileAppender for runtime log rotation.
/// Set by init_tracing() and read by the LogRotate IPC handler.
static FILE_APPENDER: std::sync::OnceLock<Arc<rollball_core::logging::SizeRollingFileAppender>> =
    std::sync::OnceLock::new();

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

    /// Debug WebSocket server port (used with --dev-mode).
    /// Gateway assigns a unique port per agent to avoid conflicts.
    /// Defaults to 19878 when not specified.
    #[arg(long, default_value = "19878")]
    pub debug_port: u16,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", env = "ROLLBALL_LOG_LEVEL")]
    pub log_level: String,

    /// Log file maximum size in MB before auto-split (0 = no split, default 10)
    #[arg(long, default_value = "10", env = "ROLLBALL_LOG_FILE_SIZE_MB")]
    pub log_file_size_mb: u64,

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
        // Initialize tracing/logging and obtain reload handle
        let reload_handle = self.init_tracing();

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
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&self.log_level));

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
                        .compact()
                )
                .init();
            return Some(reload_handle);
        }

        let max_mb = if self.log_file_size_mb > 0 { self.log_file_size_mb } else { 10 };
        let file_appender =
            Arc::new(rollball_core::logging::SizeRollingFileAppender::new(log_dir, max_mb));
        // Store for LogRotate IPC handler
        let _ = FILE_APPENDER.set(file_appender.clone());

        let (filter, reload_handle) = reload::Layer::new(env_filter);

        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_thread_ids(false)
            .with_file(false)
            .with_ansi(cfg!(not(windows))) // Enable ANSI on non-Windows, disable on Windows
            .compact();

        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_appender)
            .with_target(true)
            .with_thread_ids(true)
            .with_file(true)
            .with_ansi(false);

        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();

        Some(reload_handle)
    }
}

/// Attempt to connect to Gateway via the given socket path.
/// Returns Some((client, config)) on success, None on failure (graceful fallback to standalone mode).
async fn connect_gateway_client(
    endpoint: &str,
    agent_id: &str,
    version: &str,
) -> Option<(crate::grpc::client::GatewayGrpcClient, crate::grpc::client::AgentHelloConfig)> {
    match crate::grpc::client::GatewayGrpcClient::connect_and_register(
        endpoint, agent_id, version,
    ).await {
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

/// Async entry point after tokio runtime is initialized
async fn async_main(config: RuntimeConfig, log_reload_handle: Option<LogReloadHandle>) -> Result<()> {
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt_with_mode;
    use crate::agent::context::ContextBuilder;
    use crate::agent::loop_::AgentLoop;
    use crate::tools::builtin;
    use crate::tools::registry::ToolRegistry;

    // Step 0 (DevMode): Start debug protocol server as early as possible.
    //
    // The debug WS must be available before the frontend debug panel
    // tries to connect — which happens immediately after Gateway's
    // start_agent HTTP response.  By starting the TCP listener here
    // (before package loading, gRPC, skills, etc.) we eliminate
    // ~400ms of latency where the debug panel shows "Connecting…".
    // The debug_ctrl handles are stored and injected into AgentCore
    // later (after it is created).
    let mut early_debug_ctrl: Option<Arc<tokio::sync::Mutex<crate::debug::controller::DebugController>>> = None;
    let mut early_debug_event_tx: Option<crate::debug::server::DebugEventSender> = None;
    let mut early_rewind_notify: Option<Arc<Notify>> = None;
    let mut early_resume_notify: Option<Arc<Notify>> = None;
    if config.dev_mode {
        tracing::info!(
            port = config.debug_port,
            "DevMode enabled, starting debug protocol server on ws://127.0.0.1:{}",
            config.debug_port
        );
        let debug_server = crate::debug::server::DebugProtocolServer::new(config.debug_port);
        let (debug_event_tx, debug_ctrl) = debug_server.start().await;

        let rewind_notify = {
            let guard = debug_ctrl.lock().await;
            guard.rewind_notify_handle()
        };
        let resume_notify = {
            let guard = debug_ctrl.lock().await;
            guard.resume_notify_handle()
        };

        early_debug_ctrl = Some(debug_ctrl);
        early_debug_event_tx = Some(debug_event_tx);
        early_rewind_notify = Some(rewind_notify);
        early_resume_notify = Some(resume_notify);
    }

    // Step 1: Load .agent package (before Gateway connection so we know agent_id)
    tracing::info!(path = %config.package_path, "Loading .agent package");
    let loaded = load_package(std::path::Path::new(&config.package_path))?;
    tracing::info!(
        agent_id = %loaded.manifest.agent_id,
        name = %loaded.manifest.name,
        "Package loaded successfully"
    );

    // Step 2: Connect to Gateway gRPC if socket path is provided
    //
    // The AgentHelloResult now bundles LLM config, workspace context, and
    // runtime overrides in a single atomic response — no separate push
    // messages are needed during handshake.
    let mut grpc_client: Option<crate::grpc::client::GatewayGrpcClient> = None;
    let mut hello_config: Option<crate::grpc::client::AgentHelloConfig> = None;
    if let Some(endpoint) = config.get_gateway_address() {
        if let Some((client, cfg)) =
            connect_gateway_client(endpoint, &loaded.manifest.agent_id, &loaded.manifest.version).await
        {
            grpc_client = Some(client);
            hello_config = Some(cfg);
        }
    };
    if grpc_client.is_some() {
        tracing::info!("Gateway gRPC client initialized");
    } else {
        tracing::info!("Running in standalone mode (no Gateway)");
    }

    // Step 3: Build system prompt
    let skill_mode = resolve_skill_mode(&loaded.manifest, &config.work_dir);
    let system_prompt = build_system_prompt_with_mode(&loaded.package_dir, skill_mode)?;
    tracing::debug!(
        prompt_len = system_prompt.len(),
        "System prompt built"
    );

    // Step 3.5: Load skill registry for command-based skill injection
    // SkillRegistry is needed in the Gateway loop to inject skill instructions
    // into user messages when a command (e.g., "meeting-notes") is specified.
    let skills_dir = loaded.package_dir.join("skills");
    let skill_registry = crate::skills::parser::SkillRegistry::load_from_dir(&skills_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(
                skills_dir = %skills_dir.display(),
                error = %e,
                "Failed to load skills registry, proceeding without skills"
            );
            crate::skills::parser::SkillRegistry::new()
        });

    // Step 3: Initialize LLM Provider
    //
    // In Gateway mode: the bundled AgentHelloConfig contains LLM config,
    //   workspace context, and runtime overrides — all delivered atomically
    //   in the AgentHelloResult response.
    //
    // In Standalone mode: use manifest suggested_provider + env vars (development only).
    let mut gateway_model_capabilities: Option<rollball_core::protocol::ModelCapabilitiesInfo> = None;
    let mut gateway_max_output_tokens_limit: u64 = 32_768;
    let (provider, mut resolved_model, available_models) = if let Some(ref cfg) = hello_config {
        // Gateway mode: config was bundled in AgentHelloResult
        if let Some(ref provider_name) = cfg.provider {
            tracing::info!(
                provider = %provider_name,
                model = ?cfg.model,
                source = "AgentHelloResult",
                "LLM config received from Gateway"
            );
            gateway_model_capabilities = cfg.model_capabilities.clone();
            gateway_max_output_tokens_limit = cfg.max_output_tokens_limit;
            let p = crate::providers::router::create_provider(
                provider_name,
                &cfg.protocol_type,
                cfg.api_key.as_deref(),
                cfg.base_url.as_deref(),
            );
            // Model resolution: prefer explicit model > first from user-selected models list
            let resolved = cfg.model
                .clone()
                .or_else(|| cfg.models.first().cloned())
                .unwrap_or_else(|| {
                    tracing::error!(
                        provider = %provider_name,
                        "No model available from Gateway. \
                         Please configure a provider and select a model in Settings."
                    );
                    format!("NO_MODEL_FOR_{}", provider_name.to_uppercase())
                });
            let models = cfg.models.clone();
            (p, resolved, models)
        } else {
            // No LLM config in AgentHelloResult — fall back to noop
            tracing::error!(
                "CRITICAL: No LLM config delivered by Gateway in AgentHelloResult. \
                 Agent cannot process messages until API key is configured."
            );
            let p = crate::providers::router::create_noop_provider();
            (p, "no-model".to_string(), vec![])
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
        (p, loaded.manifest.llm.suggested_model.clone(), vec![])
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
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            tracing::warn!(
                tool = %spec.name,
                has_parameters = serialized.get("parameters").is_some(),
                has_input_schema = serialized.get("input_schema").is_some(),
                "DEBUG: Tool spec serialized fields check"
            );
            (spec.name.clone(), serialized)
        })
        .collect();
    let tool_definitions = crate::agent::context::build_tool_definitions(
        &loaded.manifest,
        &tool_specs,
    );

    // Step 6: Build context builder (with identity injection from Gateway)
    let identity_entries = load_identity_entries(&config.work_dir);
    let user_display_name = identity_entries.as_ref()
        .and_then(|entries| entries.iter()
            .find(|e| e.field == "display_name")
            .and_then(|e| if e.value.is_empty() { None } else { Some(e.value.clone()) }));
    let identity_context = identity_entries.as_ref().map(|entries| {
        if entries.is_empty() {
            return "".to_string();
        }
        let mut formatted = String::from("User identity information:\n");
        for entry in entries {
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
        formatted
    });

    // Clone tool_definitions and identity_context for SessionManagerConfig
    // (Gateway mode) before they are moved into the standalone ContextBuilder.
    let tool_definitions_for_session = tool_definitions.clone();
    let identity_context_for_session = identity_context.clone();

    let mut context_builder = ContextBuilder::new(system_prompt.clone())
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
        context_builder = context_builder.with_override_model(resolved_model.clone());
    }

    // Step 6.5: Restore per-agent model preference from workspace
    //
    // If the agent previously selected a different model (via model_switch),
    // it was persisted to .agent_model.json in the workspace.
    // On cold start, restore that preference if the model is still available.
    if let Some((saved_model, _saved_provider)) = load_agent_model(&config.work_dir) {
        if available_models.contains(&saved_model) {
            if saved_model != resolved_model {
                tracing::info!(
                    saved_model = %saved_model,
                    gateway_model = %resolved_model,
                    "Restoring per-agent model preference from workspace"
                );
                context_builder.set_override_model(saved_model.clone());
                resolved_model = saved_model;
            }
        } else {
            tracing::warn!(
                saved_model = %saved_model,
                available = ?available_models,
                "Saved model no longer available, removing .agent_model.json"
            );
            remove_agent_model(&config.work_dir);
        }
    }

    tracing::info!(
        provider = %provider.name(),
        model = %resolved_model,
        available_count = available_models.len(),
        "Final model selection after per-agent preference resolution"
    );

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
    // the on_chunk mpsc channel, then relayed to Gateway via StreamChunk.
    // Tool events (ToolCall/ToolResult) are also routed through on_chunk
    // for ordering guarantee with content chunks.
    let (chunk_tx, chunk_rx) = if grpc_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::ChunkEvent>(256);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Step 2.5: Initialize conversation session
    let work_dir_path = std::path::Path::new(&config.work_dir);
    let conversations_dir = work_dir_path.join("conversations");
    std::fs::create_dir_all(&conversations_dir)?;

    // find_latest_session expects the conversations directory (it scans it directly).
    // ConversationSession::new/resume expect the workspace root (they join "conversations" internally).
    let conversation_session = if let Some(latest_id) = crate::conversation::find_latest_session(&conversations_dir) {
        tracing::info!(session_id = %latest_id, "Resuming latest conversation session");
        Some(crate::conversation::ConversationSession::resume(work_dir_path, &latest_id)?)
    } else {
        let new_id = crate::conversation::generate_session_id();
        tracing::info!(session_id = %new_id, "Creating new conversation session");
        Some(crate::conversation::ConversationSession::new(work_dir_path, &new_id, &config.agent_id)?)
    };

    // Spawn background session scan
    let conversations_dir_clone = conversations_dir.clone();
    let _session_scan_handle = tokio::spawn(async move {
        let handle = crate::conversation::scan_sessions_async(conversations_dir_clone);
        let sessions = handle.await.unwrap_or_default();
        tracing::info!(count = sessions.len(), "Background session scan complete");
    });

    // Calculate model override for SessionManagerConfig BEFORE grpc_client is moved.
    // In Gateway mode, the resolved_model takes precedence over manifest.suggested_model.
    let override_model = if grpc_client.is_some() && resolved_model != loaded.manifest.llm.suggested_model {
        Some(resolved_model.clone())
    } else {
        None
    };

    // Step 9: Run the appropriate loop based on connection mode
    if let Some(mut client) = grpc_client {
        // Gateway mode: create SessionManager for multi-session routing
        tracing::info!("Running in Gateway mode with SessionManager");

        // Extract reconnect parameters before spawning tasks
        let agent_id = config.agent_id.clone();
        let version = loaded.manifest.version.clone();
        let socket_path = config.get_gateway_address()
            .expect("gateway address must be set in Gateway mode")
            .to_string();

        // Build shared AgentCore for all sessions
        let mut core = Arc::new(AgentCore::new(
            config.clone(),
            loaded.manifest.clone(),
            provider,
            active_tools,
            chunk_tx.clone(),
        ));

        // Inject Gateway model capabilities into the shared core.
        // Arc::get_mut only succeeds when the refcount is 1, so we must use
        // `&mut core` directly (not `core.clone()` which bumps refcount to 2
        // and makes get_mut always return None).
        if let Some(c) = Arc::get_mut(&mut core) {
            if let Some(caps) = gateway_model_capabilities {
                c.update_gateway_model_capabilities(caps);
            }
            c.update_max_output_tokens_limit(gateway_max_output_tokens_limit);
            c.user_display_name = user_display_name.clone();
            // Initialize Grafeo memory store at agent workspace
            c.init_memory_store(work_dir_path);
        }

        // Inject debug controller into AgentCore (server was started in Step 0)
        if let Some(c) = Arc::get_mut(&mut core) {
            if let (Some(debug_ctrl), Some(debug_event_tx), Some(rewind_notify), Some(resume_notify)) =
                (early_debug_ctrl.take(), early_debug_event_tx.take(), early_rewind_notify.take(), early_resume_notify.take())
            {
                c.set_debug_mode(debug_ctrl, debug_event_tx, rewind_notify, resume_notify);
            }
        }

        let session_manager_config = SessionManagerConfig {
            inbound_channel_capacity: 64,
            system_prompt: system_prompt.clone(),
            per_session_budget: budget,
            history_max_tokens: config.history_max_tokens,
            keep_full_results: config.keep_full_results,
            chunk_tx,
            tool_definitions: tool_definitions_for_session,
            identity_context: identity_context_for_session,
            override_model,
        };

        let mut session_manager = SessionManager::new(core, session_manager_config);

        // Create initial session with the resumed/created conversation
        let initial_session_id = if let Some(conv) = conversation_session {
            let sid = conv.session_id().to_string();
            session_manager.create_session_with_id_and_conversation(sid.clone(), Some(conv)).await?;
            sid
        } else {
            session_manager.create_session().await?
        };
        tracing::info!(initial_session_id = %initial_session_id, "Initial session created");

        // Step 9.5: Apply workspace context and runtime overrides from AgentHelloResult
        //
        // In the atomic handshake design (Plan B), Gateway bundles all startup
        // configuration into AgentHelloResult — no separate push messages are
        // sent during handshake. We must consume these fields here so that:
        //   1. Workspace context is broadcast to the initial session.
        //   2. Runtime overrides are cached on SessionManager (so sessions
        //      created *after* this point also inherit them) and broadcast
        //      to the initial session.
        // Without this, the agent would start with no workspace info and
        // runtime overrides would fall back to defaults until a hot-reload
        // push arrives (which may never happen if settings don't change).
        if let Some(ref cfg) = hello_config {
            if let Some(ref ctx) = cfg.workspace_context_text {
                tracing::info!(
                    workspace_id = ?cfg.current_workspace_id,
                    workspace_path = ?cfg.current_workspace_path,
                    "Applying workspace context from AgentHelloResult"
                );
                session_manager.set_workspace_context(ctx.clone());
            }
            if cfg.runtime_max_output_tokens.is_some()
                || cfg.runtime_max_iterations.is_some()
                || cfg.runtime_temperature.is_some()
                || cfg.runtime_system_prompt_override.is_some()
            {
                tracing::info!(
                    max_output_tokens = ?cfg.runtime_max_output_tokens,
                    max_iterations = ?cfg.runtime_max_iterations,
                    temperature = ?cfg.runtime_temperature,
                    "Applying runtime config overrides from AgentHelloResult"
                );
                session_manager.apply_runtime_config_override(
                    cfg.runtime_max_output_tokens,
                    cfg.runtime_max_iterations,
                    cfg.runtime_temperature,
                    cfg.runtime_system_prompt_override.clone(),
                );
            }
        }

        // Spawn chunk relay task: consumes ChunkEvent from mpsc channel and
        // forwards each event to Gateway via the shared main gRPC connection.
        // No separate connection needed — gRPC HTTP/2 is full-duplex.
        let agent_id_for_relay = agent_id.clone();
        let chunk_relay = if let Some(mut chunk_rx) = chunk_rx {
            let outbound_tx = client.outbound_sender();
            Some(tokio::spawn(async move {
                tracing::info!("Chunk relay started (shared gRPC connection)");

                while let Some(event) = chunk_rx.recv().await {
                    match event {
                        crate::agent::loop_::ChunkEvent::ReasoningStarted => {
                            let params = serde_json::json!({});
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::StreamChunk(
                                    rollball_core::proto::StreamChunk {
                                        target: "http-ws".to_string(),
                                        action: "agent_reasoning_started".to_string(),
                                        params_json: params.to_string(),
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("ReasoningStarted relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::Delta(delta) => {
                            let params = serde_json::json!({
                                "content": delta,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::StreamChunk(
                                    rollball_core::proto::StreamChunk {
                                        target: "http-ws".to_string(),
                                        action: "agent_chunk".to_string(),
                                        params_json: params.to_string(),
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Chunk relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::ReasoningDelta(delta) => {
                            let params = serde_json::json!({
                                "reasoning_content": delta,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::StreamChunk(
                                    rollball_core::proto::StreamChunk {
                                        target: "http-ws".to_string(),
                                        action: "agent_chunk".to_string(),
                                        params_json: params.to_string(),
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Reasoning chunk relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::ContextUsage(ctx_info) => {
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::ContextUsageReport(
                                    rollball_core::proto::ContextUsageReportRequest {
                                        agent_id: agent_id_for_relay.clone(),
                                        context: Some((&ctx_info).into()),
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::debug!("Context usage report send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::ToolCall { name, args, id } => {
                            let parsed_args: serde_json::Value = serde_json::from_str(&args)
                                .unwrap_or_else(|_| serde_json::json!({ "raw": args }));
                            let params = serde_json::json!({
                                "name": name,
                                "params": parsed_args,
                                "tool_call_id": id,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "agent_tool_call".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Tool call relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::ToolResult { name, result, tool_call_id } => {
                            let parsed_result: serde_json::Value = serde_json::from_str(&result)
                                .unwrap_or_else(|_| serde_json::json!({ "content": result }));
                            let params = serde_json::json!({
                                "name": name,
                                "result": parsed_result,
                                "tool_call_id": tool_call_id,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "agent_tool_result".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Tool result relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::IterationLimitPaused { iteration, max_iterations } => {
                            let params = serde_json::json!({
                                "iteration": iteration,
                                "max_iterations": max_iterations,
                                "message": format!("Iteration limit reached ({}/{}). Click Continue to keep going.", iteration, max_iterations),
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "iteration_limit_paused".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Iteration limit paused relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::Done { content, message_id } => {
                            let params = serde_json::json!({
                                "content": content,
                                "message_id": message_id,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "agent_response".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::error!("Done (agent_response) relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::Error { message, message_id } => {
                            let params = serde_json::json!({
                                "content": message,
                                "message_id": message_id,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "agent_error".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::error!("Error relay send failed — main connection may be closed");
                            }
                        }
                        crate::agent::loop_::ChunkEvent::Interrupted { content } => {
                            let params = serde_json::json!({
                                "content": content,
                            });
                            let msg = rollball_core::proto::ClientMessage {
                                request_id: 0,
                                payload: Some(rollball_core::proto::client_message::Payload::IntentSend(
                                    rollball_core::proto::IntentSendRequest {
                                        target: "http-ws".to_string(),
                                        action: "agent_interrupted".to_string(),
                                        params_json: params.to_string(),
                                        r#async: false,
                                    },
                                )),
                            };
                            if outbound_tx.send(msg).await.is_err() {
                                tracing::warn!("Interrupted relay send failed — main connection may be closed");
                            }
                        }
                    }
                }
                tracing::debug!("Chunk relay task ended");
            }))
        } else {
            None
        };

        // Extract memory query receiver before passing client to the loop.
        // This avoids &mut self conflicts when tokio::select! polls both
        // recv_message() and the memory query channel.
        let memory_query_rx = client.take_memory_query_rx();

        let result = run_gateway_loop(
            &mut session_manager,
            &mut client,
            memory_query_rx,
            config.work_dir.clone(),
            socket_path.clone(),
            agent_id.clone(),
            version.clone(),
            log_reload_handle,
            skill_registry,
            initial_session_id,
        ).await;

        // Chunk relay task will end when chunk_rx is dropped (all senders dropped)
        if let Some(handle) = chunk_relay {
            let _ = handle.await;
        }

        result
    } else {
        // Standalone mode: create AgentLoop and run interactive stdin chat loop
        tracing::info!("Running in standalone mode");

        let (mut agent_loop, _inbound_tx) = AgentLoop::new(
            config.clone(),
            loaded.manifest.clone(),
            provider,
            active_tools,
            budget,
            chunk_tx,
            conversation_session,
        );

        // Initialize Grafeo memory store at agent workspace
        agent_loop.init_memory_store(work_dir_path);

        // Inject user display name from identity delivery
        agent_loop.core.user_display_name = user_display_name.clone();

        if let Some(caps) = gateway_model_capabilities {
            agent_loop.update_gateway_model_capabilities(caps);
        }
        agent_loop.update_max_output_tokens_limit(gateway_max_output_tokens_limit);

        run_chat_loop(&mut agent_loop, &mut context_builder).await
    }
}

/// Load identity delivery from the Gateway-injected `.identity_delivery.json`
/// in the agent workspace.
///
/// When Gateway spawns an Agent, it writes identity entries to this file
/// based on the agent's `identity_deps` manifest declaration.
/// The Runtime reads this file during cold start and formats it for
/// System Prompt injection.
fn load_identity_entries(work_dir: &str) -> Option<Vec<rollball_core::identity::IdentityEntry>> {
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
                    tracing::info!(
                        entries = entries.len(),
                        "Identity delivery loaded from workspace"
                    );
                    Some(entries)
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
    use crate::providers::router::{create_provider, infer_protocol_type};

    // If no multi-provider config, use simple single provider
    if manifest.llm.providers.is_empty() {
        return create_provider(
            &manifest.llm.suggested_provider,
            &infer_protocol_type(&manifest.llm.suggested_provider),
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
        let provider = create_provider(name, &infer_protocol_type(name), api_key, base_url);
        let models = vec![config.model.clone()];
        registry.register_provider(name, provider, models);
    }

    // Also register the primary provider if not already in providers map
    if !manifest.llm.providers.contains_key(&manifest.llm.suggested_provider) {
        let primary = create_provider(
            &manifest.llm.suggested_provider,
            &infer_protocol_type(&manifest.llm.suggested_provider),
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
                &infer_protocol_type(&manifest.llm.suggested_provider),
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
/// 2. Protocol-based env key (OPENAI_API_KEY / OLLAMA_API_KEY / ANTHROPIC_API_KEY)
///
/// TODO: In the future, the env key should be looked up from offline_providers.json
/// `env` field for the specific provider, with ProtocolType as fallback only.
fn resolve_api_key(manifest: &rollball_core::AgentManifest) -> Option<String> {
    if let Ok(key) = std::env::var("ROLLBALL_LLM_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }

    use crate::providers::router::infer_protocol_type;

    let protocol_type = infer_protocol_type(&manifest.llm.suggested_provider);
    let env_key = match &protocol_type {
        rollball_core::ProtocolType::Ollama => "OLLAMA_API_KEY",
        rollball_core::ProtocolType::Anthropic => "ANTHROPIC_API_KEY",
        rollball_core::ProtocolType::OpenAI => "OPENAI_API_KEY",
    };

    std::env::var(env_key).ok().filter(|k| !k.is_empty())
}

/// Run interactive stdin chat loop
async fn run_chat_loop(
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    context_builder: &mut crate::agent::context::ContextBuilder,
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

/// Run Gateway message loop — receives messages from Gateway and routes them.
///
/// This loop is **pure routing**: it never blocks on any Session's execution.
/// Messages are forwarded to the appropriate SessionHandle's inbound channel
/// and the loop immediately returns to recv the next message.
#[allow(clippy::too_many_arguments)]
async fn run_gateway_loop(
    session_manager: &mut SessionManager,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    mut memory_query_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(u64, rollball_core::proto::server_message::Payload)>>,
    work_dir: String,
    _socket_path: String,
    agent_id_for_reconnect: String,
    version_for_reconnect: String,
    log_reload_handle: Option<LogReloadHandle>,
    skill_registry: crate::skills::parser::SkillRegistry,
    initial_session_id: String,
) -> Result<()> {
    // Retrieve the provider name for budget queries
    let budget_provider = session_manager.provider_name();

    // Track the current session ID for backward compatibility.
    // When a message does not specify session_id, it routes here.
    // Initialize with the initial session created on startup.
    let mut current_session_id = initial_session_id;

    tracing::info!("Gateway message loop started (pure routing mode)");

    // Main message loop — receive messages from Gateway and route them.
    // Also polls the memory query channel for HTTP→Runtime memory API requests.
    loop {
        if let Some(ref mut mq_rx) = memory_query_rx {
            tokio::select! {
                recv_result = grpc_client.recv_message() => {
                    match process_gateway_recv(
                        recv_result,
                        session_manager,
                        grpc_client,
                        &work_dir,
                        &agent_id_for_reconnect,
                        &version_for_reconnect,
                        &mut current_session_id,
                        &skill_registry,
                        &budget_provider,
                        &log_reload_handle,
                    ).await {
                        LoopAction::Continue => continue,
                        LoopAction::Break => break,
                    }
                }
                query_opt = mq_rx.recv() => {
                    match query_opt {
                        Some((request_id, payload)) => {
                            // Spawn to a separate task so Grafeo queries don't block
                            // the select! loop from processing Gateway messages (session
                            // refresh, etc.). The task holds cloned Arc/Sender handles.
                            let store_opt = session_manager.memory_store().cloned();
                            let outbound = grpc_client.outbound_sender();
                            tokio::spawn(spawn_memory_query_handler(
                                store_opt,
                                outbound,
                                request_id,
                                payload,
                            ));
                        }
                        None => {
                            tracing::warn!("Memory query channel closed unexpectedly");
                            memory_query_rx = None;
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
                &agent_id_for_reconnect,
                &version_for_reconnect,
                &mut current_session_id,
                &skill_registry,
                &budget_provider,
                &log_reload_handle,
            ).await {
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
    if let Some(store) = session_manager.memory_store() {
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
enum LoopAction {
    Continue,
    Break,
}

// ── Gateway message processor ───────────────────────────────────────────────

/// Process a single recv_message() result from the Gateway gRPC connection.
/// Returns LoopAction::Continue to keep looping, LoopAction::Break to exit.
#[allow(clippy::too_many_arguments)]
async fn process_gateway_recv(
    recv_result: std::result::Result<Option<rollball_core::protocol::GatewayResponse>, rollball_core::error::RollballError>,
    session_manager: &mut SessionManager,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    work_dir: &str,
    agent_id_for_reconnect: &str,
    version_for_reconnect: &str,
    current_session_id: &mut String,
    skill_registry: &crate::skills::parser::SkillRegistry,
    budget_provider: &str,
    log_reload_handle: &Option<LogReloadHandle>,
) -> LoopAction {
    use rollball_core::protocol::GatewayResponse;

    match recv_result {
        Ok(Some(response)) => {
            tracing::debug!("Received Gateway message: {:?}", response);

            match response {
                GatewayResponse::IntentReceived { from, action, params, command } => {
                    tracing::info!("Received intent from {}: {}", from, action);

                    // Determine target session: explicit session_id param > current_session_id
                    let target_session_id = params.get("session_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| current_session_id.clone());

                    // Handle model_switch: broadcast to all sessions AND
                    // update SessionManagerConfig so new sessions inherit the model.
                    // Without the config update, a session created after model switch
                    // would fall back to the stale override_model from AgentHelloResult.
                    if action == "model_switch" {
                        if let Some(model) = params.get("model").and_then(|v| v.as_str()) {
                            let provider = params.get("provider").and_then(|v| v.as_str());
                            save_agent_model(work_dir, model, provider);
                            session_manager.update_model_override(model.to_string());
                            tracing::info!(
                                model = %model,
                                provider = ?provider,
                                "Model switched via model_switch message (broadcast to all sessions)"
                            );
                        } else {
                            tracing::warn!(
                                "model_switch message missing 'model' field, ignoring"
                            );
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
                        let reason = params.get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        tracing::info!(reason = %reason, session_id = %target_session_id, "Routing interrupt to session");
                        match session_manager.get_session(&target_session_id) {
                            Some(handle) => {
                                if let Err(e) = handle.send_inbound(InboundMessage::Interrupt { reason }) {
                                    tracing::warn!("Failed to deliver interrupt to AgentLoop: {}", e);
                                }
                            }
                            None => {
                                tracing::warn!(session_id = %target_session_id, "Interrupt target session not found");
                            }
                        }
                        return LoopAction::Continue;
                    }

                    if action == "continue_execution" {
                        let reason = params.get("reason")
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
                                if let Err(e) = handle.send_inbound(InboundMessage::ContinueExecution { reason }) {
                                    tracing::warn!("Failed to deliver continue signal to AgentLoop: {}", e);
                                }
                            }
                            None => {
                                tracing::warn!(session_id = %target_session_id, "Continue target session not found");
                            }
                        }
                        return LoopAction::Continue;
                    }

                    // S1.14: Session query actions from Gateway HTTP API
                    if action == "list_sessions" {
                        handle_list_sessions(work_dir, grpc_client, &params).await;
                        return LoopAction::Continue;
                    }
                    if action == "get_session_messages" {
                        handle_get_session_messages(work_dir, grpc_client, &params).await;
                        return LoopAction::Continue;
                    }
                    if action == "create_session" {
                        let request_id = params.get("request_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let new_session_id = crate::conversation::generate_session_id();
                        match crate::conversation::ConversationSession::new(
                            std::path::Path::new(work_dir),
                            &new_session_id,
                            agent_id_for_reconnect,
                        ) {
                            Ok(new_session) => {
                                if let Err(e) = session_manager.create_session_with_id_and_conversation(
                                    new_session_id.clone(),
                                    Some(new_session),
                                ).await {
                                    tracing::error!("Failed to create session: {}", e);
                                    let data = serde_json::json!({ "error": format!("Failed to create session: {}", e) });
                                    send_session_response(grpc_client, &request_id, data).await;
                                } else {
                                    *current_session_id = new_session_id.clone();
                                    tracing::info!(new_session_id = %new_session_id, "Created new session via Gateway request");
                                    let data = serde_json::json!({ "session_id": new_session_id });
                                    send_session_response(grpc_client, &request_id, data).await;
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to create new session: {}", e);
                                let data = serde_json::json!({ "error": format!("Failed to create session: {}", e) });
                                send_session_response(grpc_client, &request_id, data).await;
                            }
                        }
                        return LoopAction::Continue;
                    }
                    if action == "get_current_session_id" {
                        handle_get_current_session_id(grpc_client, &params, current_session_id).await;
                        return LoopAction::Continue;
                    }
                    if action == "activate_session" {
                        let request_id = params.get("request_id")
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
                        // In multi-session mode, activation updates current_session_id for routing
                        *current_session_id = session_id.clone();
                        let data = serde_json::json!({
                            "session_id": session_id,
                            "activated": true,
                        });
                        send_session_response(grpc_client, &request_id, data).await;
                        return LoopAction::Continue;
                    }
                    if action == "update_session_title" {
                        let request_id = params.get("request_id")
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
                        if let Err(e) = session_manager.send_to_session(&target_session_id, SessionMessage::UpdateSessionTitle { title: title.clone() }) {
                            tracing::warn!("Failed to route update_session_title: {}", e);
                            let data = serde_json::json!({ "error": format!("Session not found: {}", target_session_id) });
                            send_session_response(grpc_client, &request_id, data).await;
                        } else {
                            let data = serde_json::json!({
                                "session_id": target_session_id,
                                "title": title,
                                "updated": true,
                            });
                            send_session_response(grpc_client, &request_id, data).await;
                        }
                        return LoopAction::Continue;
                    }
                    if action == "delete_session" {
                        let request_id = params.get("request_id")
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

                        // Delete the JSONL file
                        let conversations_dir = std::path::Path::new(work_dir).join("conversations");
                        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
                        if file_path.exists() {
                            if let Err(e) = std::fs::remove_file(&file_path) {
                                tracing::error!(session_id = %session_id, error = %e, "Failed to delete session file");
                                let data = serde_json::json!({ "error": format!("Failed to delete session: {}", e) });
                                send_session_response(grpc_client, &request_id, data).await;
                                return LoopAction::Continue;
                            }
                            tracing::info!(session_id = %session_id, "Deleted session JSONL file");
                        }

                        // Destroy the session task
                        let is_current = *current_session_id == session_id;
                        if let Err(e) = session_manager.destroy_session(&session_id).await {
                            tracing::warn!("Failed to destroy session {}: {}", session_id, e);
                        }

                        // If the deleted session was current, create a replacement
                        if is_current {
                            let new_session_id = crate::conversation::generate_session_id();
                            match crate::conversation::ConversationSession::new(
                                std::path::Path::new(work_dir),
                                &new_session_id,
                                agent_id_for_reconnect,
                            ) {
                                Ok(new_session) => {
                                    if let Err(e) = session_manager.create_session_with_id_and_conversation(
                                        new_session_id.clone(),
                                        Some(new_session),
                                    ).await {
                                        tracing::error!("Failed to create replacement session: {}", e);
                                    } else {
                                        *current_session_id = new_session_id.clone();
                                        tracing::info!(new_session_id = %new_session_id, "Switched to new session after deletion");
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create replacement session: {}", e);
                                }
                            }
                        }

                        let data = serde_json::json!({
                            "deleted": true,
                            "session_id": session_id,
                            "new_session_id": if is_current { current_session_id.clone() } else { String::new() },
                        });
                        send_session_response(grpc_client, &request_id, data).await;
                        return LoopAction::Continue;
                    }

                    // Budget pre-check: skip processing if budget is exhausted.
                    if let Ok((remaining_tokens, _)) = grpc_client.query_budget(budget_provider).await
                        && remaining_tokens == 0
                    {
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
                        let _ = grpc_client.send_intent(&from, "agent_error", error_params, false).await;
                        return LoopAction::Continue;
                    }

                    // Extract message content from params
                    let content = params.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // If a command is specified, resolve skill instructions.
                    // Instructions are passed separately (via ContextBuilder / system prompt)
                    // instead of being prepended to the user message, making them
                    // visible in the debug panel's context snapshot.
                    let skill_instructions = if let Some(skill_name) = command {
                        if let Some(skill) = skill_registry.get(&skill_name) {
                            tracing::info!(
                                skill = %skill_name,
                                "Resolved skill instructions for ContextBuilder injection"
                            );
                            Some(skill.instructions.clone())
                        } else {
                            tracing::warn!(
                                skill = %skill_name,
                                "Command skill not found in registry"
                            );
                            None
                        }
                    } else {
                        None
                    };

                    let message_id = params.get("message_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("msg-{}", chrono::Utc::now().timestamp_millis()));

                    // Pure routing: send to session's inbound channel, immediately return
                    if let Err(e) = session_manager.send_to_session(&target_session_id, SessionMessage::ChatMessage {
                        content,
                        message_id: message_id.clone(),
                        skill_instructions,
                    }) {
                        tracing::error!("Failed to route message to session {}: {}", target_session_id, e);
                        let error_params = serde_json::json!({
                            "content": format!("Session not found: {}", target_session_id),
                            "message_id": message_id,
                        });
                        let _ = grpc_client.send_intent(&from, "agent_error", error_params, false).await;
                    }
                    return LoopAction::Continue;
                }
                GatewayResponse::LLMConfigDelivery { provider, model, api_key, base_url, models: available_models, model_capabilities, max_output_tokens_limit, protocol_type, .. } => {
                        tracing::info!(
                            provider = %provider,
                            model = ?model,
                            max_output_tokens_limit = max_output_tokens_limit,
                            "Received LLMConfigDelivery at runtime — caching and broadcasting to all sessions"
                        );

                        // Model resolution: prefer explicit model > first from user-selected models
                        let resolved_model = model
                            .or_else(|| available_models.first().cloned())
                            .unwrap_or_else(|| {
                                tracing::error!(
                                    provider = %provider,
                                    "No model available from Gateway hot-push. \
                                     Please configure a provider and select a model in Settings."
                                );
                                format!("NO_MODEL_FOR_{}", provider.to_uppercase())
                            });

                        // Delegate to SessionManager: it caches the config for new sessions
                        // AND broadcasts to all existing sessions. Follows the same
                        // cache+broadcast pattern as RuntimeConfigOverrides.
                        session_manager.update_llm_config(
                            provider,
                            protocol_type,
                            api_key,
                            base_url,
                            resolved_model,
                            model_capabilities,
                            max_output_tokens_limit,
                        );
                        return LoopAction::Continue;
                    }
                    GatewayResponse::WorkspaceContextUpdate {
                        context_text,
                        current_workspace_id,
                        current_workspace_path,
                    } => {
                        tracing::info!(
                            current_id = ?current_workspace_id,
                            current_path = ?current_workspace_path,
                            "Received WorkspaceContextUpdate from Gateway — broadcasting to all sessions"
                        );
                        session_manager.set_workspace_context(context_text);
                        return LoopAction::Continue;
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
                        return LoopAction::Continue;
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
                        return LoopAction::Continue;
                    }
                    GatewayResponse::RuntimeConfigUpdate {
                        max_output_tokens,
                        max_iterations,
                        temperature,
                        system_prompt_override,
                    } => {
                        tracing::info!(
                            max_output_tokens = ?max_output_tokens,
                            max_iterations = ?max_iterations,
                            temperature = ?temperature,
                            "Received RuntimeConfigUpdate from Gateway — applying to current and future sessions"
                        );
                        // Use `apply_runtime_config_override` (not raw `broadcast`)
                        // so the override is also cached on the SessionManager
                        // and replayed to sessions created *after* this push.
                        // Otherwise the untouched `Arc<AgentCore>` template would
                        // silently revert values like `max_iterations` back to
                        // the default (50) for every brand-new session.
                        session_manager.apply_runtime_config_override(
                            max_output_tokens,
                            max_iterations,
                            temperature,
                            system_prompt_override,
                        );
                        return LoopAction::Continue;
                    }
                    _ => {
                        tracing::debug!("Ignoring non-IntentReceived Gateway message");
                        return LoopAction::Continue;
                    }
                }
            }
            Ok(None) => {
            tracing::info!("Gateway connection closed, attempting reconnect...");
            // Try to reconnect with exponential backoff
            match try_reconnect_gateway(
                agent_id_for_reconnect,
                version_for_reconnect,
                grpc_client,
            ).await {
                Ok(()) => {
                    tracing::info!("Reconnected to Gateway successfully");
                    return LoopAction::Continue;
                }
                Err(e) => {
                    tracing::error!("Failed to reconnect to Gateway: {}", e);
                    return LoopAction::Break;
                }
            }
        }
        Err(e) => {
            tracing::error!("Gateway recv error: {}", e);
            // Don't break on transient errors — try to continue
            tokio::time::sleep(std::time::Duration::from_millis(GATEWAY_RECV_RETRY_INTERVAL_MS)).await;
            return LoopAction::Continue;
        }
    }
}

// ── Memory query handler ────────────────────────────────────────────────────

/// Handle a memory API query from Gateway (via gRPC, not IntentReceived).
///
/// Gateway HTTP handlers proxy memory requests to the Runtime through the
/// gRPC bidirectional stream using request_id correlation. This handler
/// calls GrafeoStore methods and sends the proto response back.
/// Spawned as a tokio task to handle a memory query without blocking the main
/// select! loop. Takes owned data (Arc, Sender) so it can run independently.
async fn spawn_memory_query_handler(
    memory_store: Option<Arc<rollball_grafeo::grafeo::GrafeoStore>>,
    outbound: tokio::sync::mpsc::Sender<rollball_core::proto::ClientMessage>,
    request_id: u64,
    payload: rollball_core::proto::server_message::Payload,
) {
    use rollball_core::proto;
    use rollball_core::proto::server_message::Payload as ServerPayload;

    tracing::info!(
        request_id,
        payload_type = ?std::mem::discriminant(&payload),
        memory_store = memory_store.is_some(),
        "Memory query handler spawned"
    );

    let response_payload = match payload {
        ServerPayload::MemoryNodesQuery(q) => {
            handle_memory_nodes_query(memory_store.as_ref(), q)
        }
        ServerPayload::MemoryStatsQuery(_) => {
            handle_memory_stats_query(memory_store.as_ref())
        }
        ServerPayload::MemoryDeleteQuery(q) => {
            handle_memory_delete_query(memory_store.as_ref(), q)
        }
        ServerPayload::MemoryConsolidateQuery(q) => {
            handle_memory_consolidate_query(memory_store.as_ref(), q)
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
/// Maximum number of nodes to scan without any filter (keyword or type).
/// Queries exceeding this limit are rejected to prevent unbounded memory
/// allocation and excessive CPU usage on the Runtime side.
const MAX_UNFILTERED_MEMORY_SCAN: usize = 10_000;

fn handle_memory_nodes_query(
    memory_store: Option<&Arc<rollball_grafeo::grafeo::GrafeoStore>>,
    query: rollball_core::proto::MemoryNodesQuery,
) -> rollball_core::proto::client_message::Payload {
    use rollball_core::proto;
    use rollball_core::proto::client_message::Payload as ClientPayload;

    let store = match memory_store {
        Some(s) => s,
        None => {
            tracing::warn!("MemoryNodesQuery: no Grafeo store available");
            return ClientPayload::MemoryNodesResult(proto::MemoryNodesResult {
                total: 0,
                page: query.page,
                size: query.size,
                nodes: vec![],
            });
        }
    };

    let graph = store.db().graph_store();

    // time_range filtering is not yet implemented (P1).
    // Proto carries the field for future use; warn if a non-empty value
    // was supplied so the caller doesn't silently expect filtering.
    if !query.time_range.is_empty() {
        tracing::warn!(
            time_range = %query.time_range,
            "MemoryNodesQuery: time_range filtering not yet implemented, ignoring"
        );
    }

    // Collect nodes from all memory labels
    let labels = ["Episodic", "Knowledge", "Procedural", "Autobiographical"];

    // P0: Reject unfiltered queries when the database is too large.
    // Without a filter (keyword or type), the handler scans every node
    // and builds a full Vec in memory before paginating.  This is safe
    // for small databases but becomes a denial-of-service vector when
    // the node count grows into the tens of thousands.
    let has_filter = !query.keyword.is_empty() || !query.r#type.is_empty();
    if !has_filter {
        let total_nodes: usize = labels
            .iter()
            .map(|l| graph.nodes_by_label(l).len())
            .sum();
        if total_nodes > MAX_UNFILTERED_MEMORY_SCAN {
            tracing::warn!(
                total_nodes,
                max = MAX_UNFILTERED_MEMORY_SCAN,
                "MemoryNodesQuery: rejected unfiltered scan (too many nodes)"
            );
            return ClientPayload::MemoryNodesResult(proto::MemoryNodesResult {
                total: total_nodes as u64,
                page: query.page,
                size: query.size,
                nodes: vec![],
            });
        }
    }

    let mut all_entries: Vec<proto::MemoryNodeEntry> = Vec::new();

    for label in &labels {
        // Filter by type if specified
        if !query.r#type.is_empty() && query.r#type != *label {
            continue;
        }

        let node_ids = graph.nodes_by_label(label);
        let label_node_count = node_ids.len();
        let mut matched = 0usize;
        for id in node_ids {
            if let Some(n) = store.db().get_node(id) {
                let content = extract_node_content(label, &n);

                // Keyword filter — case-insensitive substring match.
                // NOTE: This is a naive O(n·m) scan; not BM25 semantic search.
                // Adequate for the Desktop App manual-search UX where node
                // counts are expected to stay under ~10K.  Upgrade path:
                // either use Grafeo's built-in text index or delegate to
                // a dedicated full-text engine (Tantivy / Meilisearch) once
                // search latency becomes a bottleneck.
                if !query.keyword.is_empty()
                    && !content.to_lowercase().contains(&query.keyword.to_lowercase())
                {
                    continue;
                }

                let created_at = n
                    .get_property("created_at")
                    .and_then(|v| v.as_timestamp())
                    .map(|ts| ts.as_secs())
                    .unwrap_or(0) as i64;

                let last_accessed_at = n
                    .get_property("last_accessed_at")
                    .and_then(|v| v.as_timestamp())
                    .map(|ts| ts.as_secs())
                    .unwrap_or(created_at) as i64;

                let access_count = n
                    .get_property("access_count")
                    .and_then(|v| v.as_int64())
                    .unwrap_or(0) as u32;

                let confidence = n
                    .get_property("confidence")
                    .and_then(|v| v.as_float64())
                    .unwrap_or(0.0);

                let decay_score = n
                    .get_property("decay_score")
                    .and_then(|v| v.as_float64())
                    .unwrap_or(1.0);

                let status = n
                    .get_property("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Active")
                    .to_string();

                all_entries.push(proto::MemoryNodeEntry {
                    node_id: id.0,
                    node_type: label.to_string(),
                    content,
                    confidence,
                    decay_score,
                    created_at,
                    last_accessed_at,
                    access_count,
                    status,
                });
                matched += 1;
            }
        }
        tracing::info!(
            label,
            total_in_label = label_node_count,
            matched,
            "MemoryNodesQuery: label scan"
        );
    }

    let total = all_entries.len() as u64;
    let page = query.page.max(1);
    let size = query.size.max(1).min(100) as usize;
    let start = ((page - 1) as usize) * size;
    let nodes: Vec<_> = if start < all_entries.len() {
        all_entries.into_iter().skip(start).take(size).collect()
    } else {
        vec![]
    };

    tracing::info!(
        total,
        page,
        returned = nodes.len(),
        "MemoryNodesQuery: final result"
    );

    ClientPayload::MemoryNodesResult(proto::MemoryNodesResult {
        total,
        page,
        size: size as u32,
        nodes,
    })
}

/// Extract a human-readable content string from a Grafeo node.
fn extract_node_content(label: &str, n: &grafeo_core::graph::lpg::Node) -> String {
    match label {
        "Episodic" => {
            let role = n.get_property("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = n.get_property("content").and_then(|v| v.as_str()).unwrap_or("");
            format!("[{}] {}", role, content)
        }
        "Knowledge" => {
            let subject = n.get_property("subject").and_then(|v| v.as_str()).unwrap_or("");
            let predicate = n.get_property("predicate").and_then(|v| v.as_str()).unwrap_or("");
            let object = n.get_property("object").and_then(|v| v.as_str()).unwrap_or("");
            format!("{} {} {}", subject, predicate, object)
        }
        "Procedural" => {
            let name = n.get_property("name").and_then(|v| v.as_str()).unwrap_or("");
            let action = n.get_property("action_pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("When {}: {}", name, action)
        }
        "Autobiographical" => {
            let key = n.get_property("key").and_then(|v| v.as_str()).unwrap_or("");
            let value = n.get_property("value").and_then(|v| v.as_str()).unwrap_or("");
            format!("{}: {}", key, value)
        }
        _ => "Unknown".to_string(),
    }
}

/// Handle MemoryStatsQuery — get memory statistics.
fn handle_memory_stats_query(
    memory_store: Option<&Arc<rollball_grafeo::grafeo::GrafeoStore>>,
) -> rollball_core::proto::client_message::Payload {
    use rollball_core::proto;
    use rollball_core::proto::client_message::Payload as ClientPayload;
    use std::collections::HashMap;

    let store = match memory_store {
        Some(s) => s,
        None => {
            return ClientPayload::MemoryStatsResult(proto::MemoryStatsResult {
                total_nodes: 0,
                storage_bytes: 0,
                by_type: HashMap::new(),
                by_status: HashMap::new(),
                avg_decay_score: 0.0,
                index_health: "no_store".to_string(),
            });
        }
    };

    match rollball_grafeo::stats::collect_stats(store) {
        Ok(stats) => {
            let total_nodes: u64 = stats.label_counts.values().sum::<usize>() as u64;

            let by_type: HashMap<String, u64> = stats
                .label_counts
                .into_iter()
                .map(|(k, v)| (k, v as u64))
                .collect();

            let mut by_status = HashMap::new();
            by_status.insert("dormant".to_string(), stats.dormant_count as u64);
            by_status.insert("purged".to_string(), stats.purged_count as u64);

            let avg_decay_score = 0.0; // TODO P3: track in StatsCollector (rollball-grafeo stats)
            let index_health = "healthy".to_string();

            ClientPayload::MemoryStatsResult(proto::MemoryStatsResult {
                total_nodes,
                storage_bytes: 0, // TODO P3: track file size in StatsCollector
                by_type,
                by_status,
                avg_decay_score,
                index_health,
            })
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to collect memory stats");
            ClientPayload::MemoryStatsResult(proto::MemoryStatsResult {
                total_nodes: 0,
                storage_bytes: 0,
                by_type: HashMap::new(),
                by_status: HashMap::new(),
                avg_decay_score: 0.0,
                index_health: format!("error: {}", e),
            })
        }
    }
}

/// Handle MemoryDeleteQuery — delete a memory node by ID.
fn handle_memory_delete_query(
    memory_store: Option<&Arc<rollball_grafeo::grafeo::GrafeoStore>>,
    query: rollball_core::proto::MemoryDeleteQuery,
) -> rollball_core::proto::client_message::Payload {
    use rollball_core::proto;
    use rollball_core::proto::client_message::Payload as ClientPayload;

    let store = match memory_store {
        Some(s) => s,
        None => {
            return ClientPayload::MemoryDeleteResult(proto::MemoryDeleteResult {
                node_id: query.node_id,
                deleted: false,
                message: "Memory store not available".to_string(),
            });
        }
    };

    let node_id = grafeo_common::types::NodeId(query.node_id);
    match store.delete_node(node_id) {
        Ok(deleted) => ClientPayload::MemoryDeleteResult(proto::MemoryDeleteResult {
            node_id: query.node_id,
            deleted,
            message: if deleted {
                "Node deleted".to_string()
            } else {
                "Node not found".to_string()
            },
        }),
        Err(e) => ClientPayload::MemoryDeleteResult(proto::MemoryDeleteResult {
            node_id: query.node_id,
            deleted: false,
            message: format!("Error: {}", e),
        }),
    }
}

/// Handle MemoryConsolidateQuery — trigger memory consolidation.
fn handle_memory_consolidate_query(
    memory_store: Option<&Arc<rollball_grafeo::grafeo::GrafeoStore>>,
    query: rollball_core::proto::MemoryConsolidateQuery,
) -> rollball_core::proto::client_message::Payload {
    use rollball_core::proto;
    use rollball_core::proto::client_message::Payload as ClientPayload;

    let store = match memory_store {
        Some(s) => s,
        None => {
            return ClientPayload::MemoryConsolidateResult(proto::MemoryConsolidateResult {
                started: false,
                duration_ms: 0,
                episodes_consolidated: 0,
                knowledge_nodes_generated: 0,
                message: "Memory store not available".to_string(),
            });
        }
    };

    let config = rollball_grafeo::consolidation::OfflineConsolidationConfig {
        batch_size: 50,
        min_pending_age_hours: if query.force { 0 } else { 1 },
    };

    let start = std::time::Instant::now();
    match store.run_offline_consolidation(&config) {
        Ok(result) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            ClientPayload::MemoryConsolidateResult(proto::MemoryConsolidateResult {
                started: true,
                duration_ms,
                episodes_consolidated: result.upgraded as u64,
                knowledge_nodes_generated: 0, // Phase 2 consolidation doesn't generate new nodes
                message: format!(
                    "Upgraded: {}, Kept pending: {}, Marked dormant: {}",
                    result.upgraded, result.kept_pending, result.marked_dormant
                ),
            })
        }
        Err(e) => ClientPayload::MemoryConsolidateResult(proto::MemoryConsolidateResult {
            started: false,
            duration_ms: 0,
            episodes_consolidated: 0,
            knowledge_nodes_generated: 0,
            message: format!("Consolidation error: {}", e),
        }),
    }
}

// ── S1.14: Session query handlers ─────────────────────────────────────────────

/// Handle "list_sessions" action from Gateway (S1.14)
///
/// Scans the conversations directory for JSONL session files,
/// converts the results to SessionInfoDto, and sends them back
/// to Gateway via IntentSend with action "session_response".
async fn handle_list_sessions(
    work_dir: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let conversations_dir = std::path::PathBuf::from(work_dir).join("conversations");
    let handle = crate::conversation::scan_sessions_async(conversations_dir);
    let sessions = match handle.await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Failed to scan sessions: {}", e);
            vec![]
        }
    };

    let session_dtos: Vec<rollball_core::protocol::SessionInfoDto> = sessions
        .into_iter()
        .map(|s| rollball_core::protocol::SessionInfoDto {
            session_id: s.session_id,
            created_at: s.created_at,
            message_count: s.message_count,
            title: s.title,
            corrupted: s.corrupted,
        })
        .collect();

    let data = serde_json::json!({
        "sessions": session_dtos,
    });

    send_session_response(grpc_client, &request_id, data).await;
}

/// Handle "get_session_messages" action from Gateway (S1.14)
///
/// Reads paginated messages from the specified session's JSONL file
/// and sends them back to Gateway via IntentSend.
async fn handle_get_session_messages(
    work_dir: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = params.get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let cursor = params.get("cursor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let limit = params.get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as u32;

    let direction = params.get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("backward")
        .to_string();

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

    match crate::conversation::read_messages_paginated(
        &file_path,
        cursor,
        limit,
        &direction,
    ) {
        Ok(paginated) => {
            let message_dtos: Vec<rollball_core::protocol::ConversationEntryDto> = paginated
                .messages
                .into_iter()
                .map(|m| rollball_core::protocol::ConversationEntryDto {
                    id: m.id,
                    ts: m.ts,
                    role: m.role,
                    content: m.content,
                    metadata: m.metadata,
                })
                .collect();

            let data = serde_json::json!({
                "messages": message_dtos,
                "cursor": paginated.cursor,
                "has_more": paginated.has_more,
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

/// Handle "create_session" action from Gateway (S1.14)
///
/// Creates a new ConversationSession and **switches the AgentLoop's active
/// conversation** to it via `switch_conversation`. This is the only correct
/// way to activate a new session — creating a ConversationSession without
/// calling switch_conversation would cause messages to still be written to
/// the old JSONL file (P0 bug fixed in ADR-session-fix).
#[allow(dead_code)]
// Reserved for future refactoring: logic currently inlined in the Gateway message loop above.
async fn handle_create_session(
    work_dir: &str,
    agent_id: &str,
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    current_session_id: &mut String,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let new_session_id = crate::conversation::generate_session_id();

    match crate::conversation::ConversationSession::new(
        std::path::Path::new(work_dir),
        &new_session_id,
        agent_id,
    ) {
        Ok(new_session) => {
            let old_session_id = agent_loop.current_session_id().map(|s| s.to_string());

            // P0 FIX: Switch the AgentLoop's active conversation to the new session.
            // Before this fix, the new_session was dropped here (bound as `_session`),
            // leaving AgentLoop.conversation pointing to the OLD session — causing
            // all subsequent messages to be appended to the wrong JSONL file.
            agent_loop.switch_conversation(new_session);

            tracing::info!(
                new_session_id = %new_session_id,
                old_session_id = ?old_session_id,
                "Created and activated new conversation session via Gateway request"
            );
            *current_session_id = new_session_id.clone();

            let data = serde_json::json!({
                "session_id": new_session_id,
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
        Err(e) => {
            tracing::error!("Failed to create new session: {}", e);
            let data = serde_json::json!({
                "error": format!("Failed to create session: {}", e),
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
    }
}

/// Handle "get_current_session_id" action from Gateway (S1.14)
///
/// Returns the currently active session ID.
async fn handle_get_current_session_id(
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
    current_session_id: &str,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = if current_session_id.is_empty() {
        None
    } else {
        Some(current_session_id.to_string())
    };

    let data = serde_json::json!({
        "session_id": session_id,
    });
    send_session_response(grpc_client, &request_id, data).await;
}

/// Handle "activate_session" action from Gateway (S1.14)
///
/// Resumes an existing ConversationSession from its JSONL file and switches
/// the AgentLoop's active conversation to it. This is the Runtime-side
/// implementation of the session switch protocol:
///
/// 1. Frontend calls POST /api/agents/{id}/sessions/{session_id}/activate
/// 2. Gateway forwards "activate_session" IntentReceived to Runtime
/// 3. Runtime resumes the JSONL file → creates ConversationSession
/// 4. Runtime calls agent_loop.switch_conversation() to activate it
/// 5. All subsequent messages are written to the new (resumed) JSONL file
///
/// Without this, switching sessions on the frontend only updates UI state —
/// the Runtime keeps writing to whatever session it had active, causing the
/// "messages in wrong session" bug.
#[allow(dead_code)]
// Reserved for future refactoring: logic currently inlined in the Gateway message loop above.
async fn handle_activate_session(
    work_dir: &str,
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    current_session_id: &mut String,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) if !sid.is_empty() => sid.to_string(),
        _ => {
            let data = serde_json::json!({
                "error": "Missing or empty session_id parameter",
            });
            send_session_response(grpc_client, &request_id, data).await;
            return;
        }
    };

    // Resume the existing session's JSONL file
    match crate::conversation::ConversationSession::resume(
        std::path::Path::new(work_dir),
        &session_id,
    ) {
        Ok(resumed_session) => {
            let old_session_id = agent_loop.current_session_id().map(|s| s.to_string());

            // Switch the AgentLoop's active conversation
            agent_loop.switch_conversation(resumed_session);

            tracing::info!(
                activated_session_id = %session_id,
                old_session_id = ?old_session_id,
                "Activated existing conversation session via Gateway request"
            );
            *current_session_id = session_id.clone();

            let data = serde_json::json!({
                "session_id": session_id,
                "activated": true,
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
        Err(e) => {
            tracing::error!(session_id = %session_id, error = %e, "Failed to resume session for activation");
            let data = serde_json::json!({
                "error": format!("Failed to activate session: {}", e),
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
    }
}

/// Handle "update_session_title" action from Gateway (S1.14)
///
/// Force-updates the title of the currently active ConversationSession.
/// Unlike `set_title` (which only sets once from the first user message),
/// this always writes the title — used by the HTTP API for manual/programmatic
/// title updates that persist in the JSONL metadata.
#[allow(dead_code)]
// Reserved for future refactoring: logic currently inlined in the Gateway message loop above.
async fn handle_update_session_title(
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let title = match params.get("title").and_then(|v| v.as_str()) {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let data = serde_json::json!({
                "error": "Missing or empty title parameter",
            });
            send_session_response(grpc_client, &request_id, data).await;
            return;
        }
    };

    match agent_loop.update_session_title(&title) {
        Some(true) => {
            let session_id = agent_loop.current_session_id().map(|s| s.to_string()).unwrap_or_default();
            let data = serde_json::json!({
                "session_id": session_id,
                "title": title,
                "updated": true,
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
        Some(false) => {
            // Title unchanged — no-op
            let session_id = agent_loop.current_session_id().map(|s| s.to_string()).unwrap_or_default();
            let data = serde_json::json!({
                "session_id": session_id,
                "title": title,
                "updated": false,
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
        None => {
            tracing::warn!("update_session_title called but no active session");
            let data = serde_json::json!({
                "error": "No active session to update",
            });
            send_session_response(grpc_client, &request_id, data).await;
        }
    }
}

/// Handle "delete_session" action from Gateway
///
/// Deletes the session's JSONL file. If the deleted session is the currently
/// active one, creates a new session and switches to it automatically.
#[allow(dead_code)]
// Reserved for future refactoring: logic currently inlined in the Gateway message loop above.
async fn handle_delete_session(
    work_dir: &str,
    agent_loop: &mut crate::agent::loop_::AgentLoop,
    current_session_id: &mut String,
    agent_id: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
    params: &serde_json::Value,
) {
    let request_id = params.get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = match params.get("session_id").and_then(|v| v.as_str()) {
        Some(sid) if !sid.is_empty() => sid.to_string(),
        _ => {
            let data = serde_json::json!({
                "error": "Missing or empty session_id parameter",
            });
            send_session_response(grpc_client, &request_id, data).await;
            return;
        }
    };

    let conversations_dir = std::path::Path::new(work_dir).join("conversations");
    let file_path = conversations_dir.join(format!("{}.jsonl", session_id));

    // Delete the JSONL file
    if file_path.exists() {
        if let Err(e) = std::fs::remove_file(&file_path) {
            tracing::error!(session_id = %session_id, error = %e, "Failed to delete session file");
            let data = serde_json::json!({
                "error": format!("Failed to delete session: {}", e),
            });
            send_session_response(grpc_client, &request_id, data).await;
            return;
        }
        tracing::info!(session_id = %session_id, "Deleted session JSONL file");
    }

    // If the deleted session was the currently active one, create a new session
    let is_current = *current_session_id == session_id;
    if is_current {
        let new_session_id = crate::conversation::generate_session_id();
        match crate::conversation::ConversationSession::new(
            std::path::Path::new(work_dir),
            &new_session_id,
            agent_id,
        ) {
            Ok(new_session) => {
                agent_loop.switch_conversation(new_session);
                *current_session_id = new_session_id.clone();
                tracing::info!(new_session_id = %new_session_id, "Switched to new session after deletion");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to create new session after deletion");
            }
        }
    }

    let data = serde_json::json!({
        "deleted": true,
        "session_id": session_id,
        "new_session_id": if is_current { current_session_id.clone() } else { String::new() },
    });
    send_session_response(grpc_client, &request_id, data).await;
}

/// Send a session response back to Gateway via IntentSend (S1.14)
///
/// Wraps the response data with the request_id and sends it
/// as an IntentSend with action "session_response" targeting "http-api".
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
        // Use a non-existent socket path to force connection failure.
        // Use connect_with_timeout directly to avoid the default 300s
        // gRPC connect retry budget.
        let result = crate::grpc::client::GatewayGrpcClient::connect_with_timeout(
            "unix:///nonexistent/socket/path.sock",
            2, // 2-second max elapsed time — enough to try a few times
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
fn resolve_skill_mode(
    manifest: &rollball_core::AgentManifest,
    work_dir: &str,
) -> rollball_core::SkillMode {
    let default_progressive = manifest.skills.progressive;

    // Check for user override in workspace
    let override_path = std::path::Path::new(work_dir).join(".agent_skills.json");
    if override_path.exists() {
        match std::fs::read_to_string(&override_path) {
            Ok(content) => {
                match serde_json::from_str::<AgentSkillsOverride>(&content) {
                    Ok(override_config) => {
                        if let Some(progressive) = override_config.progressive {
                            tracing::info!(
                                progressive = %progressive,
                                manifest_default = %default_progressive,
                                "Skill mode overridden by .agent_skills.json"
                            );
                            return if progressive {
                                rollball_core::SkillMode::Progressive
                            } else {
                                rollball_core::SkillMode::Manual
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
                }
            }
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

// ── Per-agent model persistence ──────────────────────────────────────────

/// Agent model preference file stored in the workspace directory.
///
/// When the user switches models via model_switch, the Agent Runtime persists
/// the selection to this file so it survives restarts. On cold start, the Runtime
/// reads this file and restores the preference if the model is still available
/// in the LLMConfigDelivery.models list.
const AGENT_MODEL_FILE: &str = ".agent_model.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct AgentModelEntry {
    /// The model identifier selected by the user
    model: String,
    /// The provider identifier selected by the user (e.g., "deepseek", "openai")
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// ISO 8601 timestamp of when the model was last changed
    updated_at: String,
}

/// Load the per-agent model preference from the workspace.
///
/// Returns `Some((model, provider))` if the file exists and parses correctly, otherwise `None`.
/// The provider field may be `None` for legacy files that only stored the model.
fn load_agent_model(work_dir: &str) -> Option<(String, Option<String>)> {
    let path = std::path::Path::new(work_dir).join(AGENT_MODEL_FILE);
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            match serde_json::from_str::<AgentModelEntry>(&content) {
                Ok(entry) => {
                    tracing::info!(
                        model = %entry.model,
                        provider = ?entry.provider,
                        updated_at = %entry.updated_at,
                        "Loaded per-agent model preference from workspace"
                    );
                    Some((entry.model, entry.provider))
                }
                Err(e) => {
                    tracing::warn!("Failed to parse {}: {}", AGENT_MODEL_FILE, e);
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to read {}: {}", AGENT_MODEL_FILE, e);
            None
        }
    }
}

/// Save the per-agent model preference to the workspace.
///
/// Called when a model_switch message is received. Overwrites any existing entry.
/// The provider is saved alongside the model so that the correct provider can be
/// restored when the agent restarts or when the frontend queries the model info.
fn save_agent_model(work_dir: &str, model: &str, provider: Option<&str>) {
    let entry = AgentModelEntry {
        model: model.to_string(),
        provider: provider.map(|s| s.to_string()),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let path = std::path::Path::new(work_dir).join(AGENT_MODEL_FILE);
    match serde_json::to_string_pretty(&entry) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!("Failed to write {}: {}", AGENT_MODEL_FILE, e);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to serialize {}: {}", AGENT_MODEL_FILE, e);
        }
    }
}

/// Remove the per-agent model preference file from the workspace.
///
/// Called when the saved model is no longer available in the current provider's
/// model list (e.g. provider was changed or model was removed).
fn remove_agent_model(work_dir: &str) {
    let path = std::path::Path::new(work_dir).join(AGENT_MODEL_FILE);
    if path.exists()
        && let Err(e) = std::fs::remove_file(&path)
    {
        tracing::warn!("Failed to remove {}: {}", AGENT_MODEL_FILE, e);
    }
}

/// Attempt to reconnect to the Gateway via gRPC with exponential backoff.
///
/// Called when the gRPC connection drops (Gateway restart, network issue, etc.).
/// Returns Ok(()) if reconnection succeeds, Err if all attempts fail.
async fn try_reconnect_gateway(
    agent_id: &str,
    version: &str,
    grpc_client: &mut crate::grpc::client::GatewayGrpcClient,
) -> Result<()> {
    match grpc_client.reconnect_and_reregister(agent_id, version).await {
        Ok(()) => {
            tracing::info!("Reconnected to Gateway gRPC successfully");
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to reconnect to Gateway gRPC: {}", e);
            Err(crate::error::RuntimeError::Ipc(format!(
                "gRPC reconnect failed: {}", e
            )))
        }
    }
}
