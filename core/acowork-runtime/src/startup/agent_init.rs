//! Phase A: per-agent initialization.
//!
//! Covers Steps 0-8 of the original `async_main`:
//!   - Load .agent package
//!   - Connect to Gateway gRPC + AgentHello handshake
//!   - Build system prompt & SkillRegistry
//!   - Resolve LLM provider
//!   - Build FallbackEmbeddingProvider (3-tier chain)
//!   - Build ToolRegistry + activate tools
//!   - Build tool definitions
//!   - Build ContextBuilder
//!   - Create Budget + chunk_tx/chunk_rx channel

use std::sync::Arc;

use acowork_core::protocol::ProtocolType;

use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::AgentBootContext;

/// Phase A: initialize all per-agent (cross-session) resources.
///
/// This function is the first phase of the agent startup sequence.
/// It produces an `AgentBootContext` that is consumed by subsequent phases.
pub(crate) async fn phase_a_init_agent(config: &RuntimeConfig) -> Result<AgentBootContext> {
    let _span = tracing::info_span!("startup_phase_a").entered();

    use crate::agent::context::ContextBuilder;
    use crate::embedding::remote::RemoteEmbeddingProvider;
    use crate::embedding::EmbeddingProvider;
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt_with_mode;
    use crate::startup::super_mod::{
        read_resource_cache,
    };
    use crate::tools::builtin;
    use crate::tools::registry::ToolRegistry;

    // ── Step 1: Load .agent package ─────────────────────────────────
    tracing::info!(path = %config.package_path, "Loading .agent package");
    let loaded = load_package(std::path::Path::new(&config.package_path))?;
    tracing::info!(
        agent_id = %loaded.manifest.agent_id,
        name = %loaded.manifest.name,
        "Package loaded successfully"
    );

    // ── ADR-040: gRPC path removed. Only MQTT transport remains. ─────

    // ── ADR-033: Start MQTT client + HTTP server ─────────────────────
    let mut mqtt_client: Option<crate::mqtt::RuntimeMqttClient> = None;
    let mut available_cache: Option<crate::mqtt::SharedAvailableCache> = None;
    let mut control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>> = None;
    let mut runtime_http_port: Option<u16> = None;

    // Shared session snapshot map, created here so it can be passed to both
    // the HTTP server (Phase A) and SessionManager (Phase B). The same Arc
    // is shared, so session state writes are immediately visible to HTTP reads.
    let session_snapshots: crate::agent::session_state::SharedSessionSnapshots =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // Shared latest session Arc, written by SessionManager on every session
    // creation and startup scan, read by the HTTP /sessions/latest endpoint.
    let latest_session: crate::agent::session_state::SharedLatestSession =
        Arc::new(std::sync::RwLock::new(None));

    // ADR-033: Dispatch channel for Runtime HTTP → agent loop write operations.
    // HTTP handlers send (session_id, InboundMessage) tuples; the gateway loop
    // forwards them to the right session's AgentLoop via send_inbound().
    let (http_dispatch_tx, http_dispatch_rx) = tokio::sync::mpsc::unbounded_channel::<(String, crate::agent::inbound::InboundMessage)>();
    let http_dispatch_shared: crate::http::SharedDispatchSender =
        Arc::new(tokio::sync::Mutex::new(Some(http_dispatch_tx)));

    // ADR-033: Shared handle to the Grafeo memory store. Created empty here
    // and populated by Phase B (`init_memory_store`) so the HTTP server can
    // start serving requests before the store is ready. The HTTP handlers
    // return a stable "no store" response when this is still `None`.
    let memory_store_shared: crate::http::SharedMemoryStore =
        Arc::new(std::sync::RwLock::new(None));

    // Shared embedding-provider dimension. Starts at 0 (no provider) and
    // is updated once `embed_dimension` is resolved below (after the HTTP
    // server has already started listening). The memory-stats handler
    // surfaces this as `model_dim` for HNSW dimension-mismatch detection.
    let embed_dim_shared: crate::http::SharedEmbedDimension =
        Arc::new(std::sync::RwLock::new(0));

    // Shared degradation reasons. Created empty here and populated by
    // Phase B if session persistence fails. The same Arc is passed to
    // the HTTP server so `/health` can surface non-fatal startup errors.
    let degraded_reasons: crate::http::SharedDegradation =
        Arc::new(std::sync::RwLock::new(Vec::new()));

    // ADR-028: Late-bind slot for the AgentCore. Created empty in Phase A
    // and populated by Phase B once `AgentCore::new` completes. The
    // `list_sessions` HTTP handler uses it to merge disk-scanned token
    // totals and report `agent_total_input_tokens` / `agent_total_output_tokens`.
    let agent_core_shared: crate::http::SharedAgentCore =
        Arc::new(std::sync::RwLock::new(None));

    // ADR-040: Late-bind slot for session metadata service. Populated
    // by Phase B. The `list_sessions` handler falls back to direct
    // implementation when this is still None.
    let session_metadata_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // ADR-040: Late-bind slot for memory query service.
    let memory_query_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // ADR-040: Late-bind slots for workspace query + mutation services.
    // Workspace services are populated immediately after the runtime
    // boots (no async dependency like memory) — see session_init.rs.
    let workspace_query_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // ADR-038-style late-bind slot for the MQTT client. The Runtime HTTP
    // server starts here in Phase A before `mqtt_client` is connected, so
    // we hand the server an `Arc<Mutex<Option<_>>>` slot and populate it
    // once the broker connection succeeds. The `PUT /agents/{id}/config`
    // handler uses it to re-PUBLISH the retained AgentConfig snapshot.
    let mqtt_client_slot: crate::http::server::SharedMqttClientSlot =
        Arc::new(tokio::sync::Mutex::new(None));

    if let Some(_http_port) = config.http_port {
        match crate::http::RuntimeHttpServer::start(
            std::path::PathBuf::from(&config.work_dir),
            loaded.manifest.agent_id.clone(),
            session_snapshots.clone(),
            latest_session.clone(),
            http_dispatch_shared,
            embed_dim_shared.clone(),
            degraded_reasons.clone(),
            mqtt_client_slot.clone(),
            session_metadata_slot.clone(),
            memory_query_slot.clone(),
            workspace_query_slot.clone(),
            workspace_mutation_slot.clone(),
        ).await {
            Ok(server) => {
                runtime_http_port = Some(server.port);
                tracing::info!(port = server.port, "Runtime HTTP server started");
                std::mem::forget(server);
            }
            Err(e) => tracing::warn!(error = %e, "Runtime HTTP server start failed"),
        }
    }

    if let Some(mqtt_port) = config.mqtt_port {
        let cache = crate::mqtt::new_shared_cache();
        let (control_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel();
        let config_json = crate::agent_config::load_agent_config(std::path::Path::new(&config.work_dir))
            .ok().flatten().map(|c| serde_json::to_string(&c).unwrap_or_default()).unwrap_or_default();
        match crate::mqtt::RuntimeMqttClient::connect(
            crate::mqtt::client::MqttConnectConfig {
                host: "127.0.0.1",
                port: mqtt_port,
                agent_id: &loaded.manifest.agent_id,
                agent_name: &loaded.manifest.name,
                agent_version: &loaded.manifest.version,
                avatar: None,
                builtin_avatar: None,
                config_json: &config_json,
                available_cache: cache.clone(),
                control_tx,
            },
        ).await {
            Ok(client) => {
                tracing::info!(agent_id=%loaded.manifest.agent_id, "Runtime MQTT client connected");
                // Publish HTTP port (Retained) so Gateway can proxy session queries.
                // Surface publish errors instead of silently swallowing them — a failed publish
                // is the most likely cause of the Gateway returning 503 with latency=0ms
                // (the Gateway subscribes to this retained topic on startup; if the publish
                // never lands, the registry stays empty and every reverse-proxy request 503s).
                if let Some(port) = runtime_http_port {
                    let topic = format!("acowork/agents/{}/http_port", loaded.manifest.agent_id);
                    match client.publish_raw(
                        &topic,
                        port.to_string().as_bytes(),
                        crate::mqtt::client::MqttQoS::AtLeastOnce,
                        true, // Retained — so Gateway can discover on restart
                    ).await {
                        Ok(()) => tracing::info!(
                            agent_id=%loaded.manifest.agent_id,
                            port,
                            "Published retained http_port for Gateway reverse-proxy discovery"
                        ),
                        Err(e) => tracing::error!(
                            agent_id=%loaded.manifest.agent_id,
                            port,
                            error=%e,
                            "Failed to publish retained http_port — Gateway will return 503 until the Runtime restarts and re-publishes"
                        ),
                    }
                }
                // ADR-038-style: fill the late-bind slot so the HTTP
                // server's `PUT /agents/{id}/config` handler can re-PUBLISH
                // the retained AgentConfig snapshot when the Desktop flips a
                // builtin tool. `RuntimeMqttClient` is `Clone` (cheap Arc
                // handles for AsyncClient + event-loop guard — see
                // `mqtt/client.rs`), so we hand a clone to the slot and
                // keep the original for `AgentBootContext::mqtt_client`,
                // which downstream sites (`subsystems.rs` / `gateway_loop.rs`)
                // still consume by `&RuntimeMqttClient` reference.
                *mqtt_client_slot.lock().await = Some(Arc::new(tokio::sync::Mutex::new(client.clone())));
                mqtt_client = Some(client);
                available_cache = Some(cache);
                control_rx = Some(ctrl_rx);
            }
            Err(e) => tracing::warn!(error=%e, "MQTT client connect failed"),
        }
    }

    // ── Step 3: Build system prompt ─────────────────────────────────
    let skill_mode =
        crate::startup::super_mod::resolve_skill_mode(&loaded.manifest, &config.work_dir);
    let system_prompt = build_system_prompt_with_mode(&loaded.package_dir, skill_mode)?;
    tracing::debug!(prompt_len = system_prompt.len(), "System prompt built");

    // ── Step 3.5: Load skill registry ───────────────────────────────
    let skills_dir = loaded.package_dir.join("skills");
    let _skill_registry = crate::skills::parser::SkillRegistry::load_from_dir(&skills_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(
                skills_dir = %skills_dir.display(),
                error = %e,
                "Failed to load skills registry, proceeding without skills"
            );
            crate::skills::parser::SkillRegistry::new()
        });

    // ── Step 3: Initialize LLM Provider ─────────────────────────────
    let mut gateway_current_provider_id: Option<String> = None;
    let resource_cache = read_resource_cache(std::path::Path::new(&config.work_dir));

    let (provider, resolved_model, available_models, protocol_type) = {
        // ADR-033: MQTT available_cache is the only provider source (gRPC path removed per ADR-040).
        if let Some(ref cache) = available_cache {
            // Wait up to 10s for the first acowork/global/providers to arrive.
            // The Gateway publishes every 5s (non-retained due to rumqttd 0.14).
            let mut waited = 0;
            loop {
                let cache_read = cache.read().await;
                if cache_read.providers.is_some() || waited >= 20 {
                    break;
                }
                drop(cache_read);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                waited += 1;
            }
            let cache_read = cache.read().await;
            if let Some(ref available_providers) = cache_read.providers {
                // Use MQTT provider info. API key resolution now happens
                // via the `api_key` field embedded in the
                // `acowork/global/providers` payload itself — see
                // [global_resources_publisher] and mqtt.md §3.1.1. The
                // legacy `ACOWORK_PROVIDER_<ID>_KEY` env var is no longer
                // used because the Gateway does not inject keys into the
                // Runtime process environment (bug 1 of the
                // senior-engineer startup log).
                let chosen = available_providers.providers.first();

                if let Some(prov) = chosen {
                    gateway_current_provider_id = Some(prov.id.clone());
                    let api_key = prov.api_key.clone();
                    // Empty string means "no key configured" (e.g. local
                    // Ollama). Convert to Option<&str> for the provider
                    // factory, which treats None as "no auth header".
                    let api_key_opt: Option<&str> =
                        if api_key.is_empty() { None } else { Some(api_key.as_str()) };
                    let available = prov.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
                    let model_id = prov.models.first().map(|m| m.id.clone()).unwrap_or_else(|| "default".to_string());
                    let proto_str = ProtocolType::OpenAI; // MQTT providers are OpenAI-compatible
                    let timeouts = Some(crate::providers::router::ProviderTimeouts::from(config));
                    let provider = crate::providers::router::create_provider(
                        &prov.id,
                        &proto_str,
                        api_key_opt,
                        Some(&prov.base_url),
                        timeouts,
                    );
                    tracing::info!(
                        provider = %prov.id,
                        model = %model_id,
                        num_models = available.len(),
                        has_api_key = api_key_opt.is_some(),
                        source = "mqtt_available_cache",
                        "Provider initialized from MQTT available cache"
                    );
                    (provider, model_id, available, proto_str)
                } else {
                    tracing::warn!("MQTT providers available but none selected, using noop");
                    noop_provider_tuple()
                }
            } else {
                tracing::warn!("MQTT connected but no providers in cache yet, using noop");
                noop_provider_tuple()
            }
        } else {
            noop_provider_tuple()
        }
    };

    // ── Step 3.5: Build Embedding Provider (single-tier, no fallback) ──
    //
    // Single source: env var ACOWORK_EMBED_ENDPOINT (Gateway embed_supervisor
    // port 18080). ADR-040: gRPC hello_config path removed.
    //
    // Failure semantics:
    //   - endpoint set + provider ok → INFO ✅
    //   - endpoint set + provider fail → ERROR ❌, emb_provider = None
    //   - endpoint not set → ERROR ❌, emb_provider = None
    //
    // emb_provider = None means memory::manager auto-falls back to
    // text_search_with_filter (manager.rs L270-280).

    let embed_endpoint = std::env::var("ACOWORK_EMBED_ENDPOINT").ok();
    let embed_model_id = std::env::var("ACOWORK_EMBED_MODEL")
        .ok()
        .unwrap_or_else(|| "bge-small-zh-v1.5".to_string());
    let embed_dimension = std::env::var("ACOWORK_EMBED_DIMENSION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);

    let emb_provider: Option<Arc<dyn EmbeddingProvider>> = match embed_endpoint.as_deref() {
        Some(endpoint) => {
            tracing::info!(
                endpoint = %endpoint,
                model = %embed_model_id,
                dim = embed_dimension,
                "🔧 Building embedding provider (Tier 1: local ONNX at {})",
                endpoint
            );
            match RemoteEmbeddingProvider::try_with_config_and_timeouts(
                endpoint,
                None,
                &embed_model_id,
                embed_dimension,
                &config.timeouts,
            ) {
                Ok(ep) => {
                    let dim = ep.dimension();
                    let name = ep.name();
                    tracing::info!(
                        endpoint = %endpoint,
                        model = %embed_model_id,
                        dim = dim,
                        provider_name = %name,
                        "✅ Embedding provider initialized successfully (Tier 1: local ONNX)"
                    );
                    if let Ok(mut slot) = embed_dim_shared.write() {
                        *slot = dim as u64;
                    }
                    Some(Arc::new(ep))
                }
                Err(e) => {
                    tracing::error!(
                        endpoint = %endpoint,
                        model = %embed_model_id,
                        dim = embed_dimension,
                        error = %e,
                        "❌ Failed to build embedding provider (Tier 1: local ONNX at {}). \
                         No fallback chain — fix the embed service (acowork-embed on {}) \
                         and restart runtime. Memory will degrade to text-only search.",
                        endpoint, endpoint
                    );
                    None
                }
            }
        }
        None => {
            tracing::error!(
                "❌ ACOWORK_EMBED_ENDPOINT not set. \
                 Gateway embed_supervisor must be running before runtime starts. \
                 Memory will degrade to text-only search."
            );
            None
        }
    };

    // ── Step 4: Build tool registry + activate by manifest ──────────
    let workspace_resolver: crate::tools::workspace_resolver::SharedResolver =
        Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new(&config.work_dir),
        ));
    let has_search_providers = false;
    // ADR-040: gRPC hello_config path removed. search providers + LSP relay
    // are always unavailable; both features depend on future MQTT-side delivery.
    let lsp_relay_endpoint: Option<String> = None;

    let memory_session = Arc::new(crate::memory::MemorySessionHandle::new(
        emb_provider.clone(),
    ));
    let mcp_notifier = Arc::new(crate::mcp_notify::McpConfigNotifier::default());

    let mut registry = ToolRegistry::new();
    for tool in builtin::all_builtin_tools(
        &workspace_resolver,
        &config.agent_id,
        config.timeouts.tool_http_timeout_ms,
        has_search_providers,
        None,
        Some(memory_session.clone()),
        Some(mcp_notifier.clone()),
        config.work_dir.clone(),
        lsp_relay_endpoint,
        mqtt_client_slot.clone(),
    ) {
        registry.register(tool);
    }

    // ── ADR-029: Resolve `agent_tools.json` enabled flags ──────────
    //
    // 1. If `{work_dir}/config/agent_tools.json` exists -> load it,
    //    merge with code registry, and **persist the merged result**
    //    so new tools from code upgrades are saved to disk.
    //    Persisted `enabled` flags are always preserved (the user's
    //    choices are the single source of truth; never overwrite).
    // 2. If absent -> generate initial config from manifest `[[tools]]`
    //    (only declared tools are enabled; if no `[[tools]]` section
    //    exists, all tools default to disabled — opt-in model).
    let work_path = std::path::Path::new(&config.work_dir);
    let code_tool_list: Vec<String> = registry
        .all()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let manifest_tool_names = loaded.manifest.builtin_tool_names();

    let resolved_entries: Vec<crate::agent_config::AgentToolEntry> =
        match crate::agent_config::load_agent_tools_config(work_path) {
            Ok(Some(persisted_cfg)) => {
                tracing::info!(
                    count = persisted_cfg.tools.len(),
                    "Loaded existing agent_tools.json — merging with code registry"
                );
                let merged = crate::agent_config::merge_tools_config(
                    &code_tool_list,
                    &persisted_cfg.tools,
                );
                // Persist the merged result so new tools from code
                // upgrades are written to agent_tools.json immediately.
                // This ensures the file is always a complete,
                // up-to-date reflection of the code registry.
                if let Err(e) = crate::agent_config::save_agent_tools_config(
                    work_path,
                    &crate::agent_config::AgentToolsConfig {
                        tools: merged.clone(),
                    },
                ) {
                    tracing::warn!(
                        error = %e,
                        "Failed to persist merged agent_tools.json after startup merge"
                    );
                }
                merged
            }
            Ok(None) => {
                // First start: seed from manifest [[tools]] declarations.
                // Only tools listed in the manifest are enabled; everything
                // else is disabled (opt-in model).  If the manifest has no
                // [[tools]] section, all tools are disabled by default.
                let initial = crate::agent_config::init_tools_config_from_manifest(
                    &code_tool_list,
                    &manifest_tool_names,
                );
                tracing::info!(
                    manifest_tools = manifest_tool_names.len(),
                    enabled = initial.iter().filter(|e| e.enabled).count(),
                    disabled = initial.iter().filter(|e| !e.enabled).count(),
                    "agent_tools.json absent — seeded from manifest [[tools]]"
                );
                // Persist the freshly generated config so future startups see it
                if let Err(e) = crate::agent_config::save_agent_tools_config(
                    work_path,
                    &crate::agent_config::AgentToolsConfig {
                        tools: initial.clone(),
                    },
                ) {
                    tracing::warn!(error = %e, "Failed to persist initial agent_tools.json");
                }
                initial
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to parse agent_tools.json — falling back to manifest-derived config"
                );
                crate::agent_config::init_tools_config_from_manifest(
                    &code_tool_list,
                    &manifest_tool_names,
                )
            }
        };

    // Activate with the merged enabled list, applying security
    // decorators (path guard + rate limiter) to each tool.
    let active_tools = registry.activate(
        &loaded.manifest,
        &workspace_resolver,
        60,
        &resolved_entries,
    );
    tracing::info!(
        total_enabled = active_tools.iter().filter(|e| e.enabled).count(),
        total_disabled = active_tools.iter().filter(|e| !e.enabled).count(),
        "agent_tools.json resolved"
    );

    // ── Step 5: Build tool definitions ──────────────────────────────
    #[allow(unused_imports)]
    use acowork_core::tools::traits::Tool;
    // LLM-visible tool specs: enabled subset only.
    let tool_specs: Vec<(String, serde_json::Value)> = active_tools
        .iter()
        .filter(|e| e.enabled)
        .map(|e| {
            let spec = e.spec();
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
    let tool_definitions: Vec<serde_json::Value> =
        tool_specs.iter().map(|(_, v)| v.clone()).collect();

    // Full builtin specs: every builtin regardless of enabled (used by
    // the frontend ToolsTab — `GET /api/agents/{id}/builtin-tools`).
    let full_tool_specs: Vec<(String, serde_json::Value)> = active_tools
        .iter()
        .map(|e| {
            let spec = e.spec();
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            (spec.name.clone(), serialized)
        })
        .collect();
    tracing::info!(
        active_specs = tool_specs.len(),
        full_specs = full_tool_specs.len(),
        "Tool specs: enabled vs full builtin registry"
    );

    // ── Step 6: Build context builder ───────────────────────────────
    // ADR-040: gRPC hello_config path removed. User identity is not yet
    // available via MQTT; context builder is created without identity.
    let identity_context: Option<String> = None;

    let mut context_builder = ContextBuilder::new(system_prompt.clone())
        .with_identity(identity_context.clone())
        .with_tools(tool_definitions.clone());
    context_builder = context_builder.with_override_model(resolved_model.clone());

    tracing::info!(
        provider = %provider.name(),
        model = %resolved_model,
        available_count = available_models.len(),
        "Final model selection after per-agent preference resolution"
    );

    // ── Step 7: Create budget ────────────────────────────────────────
    let budget = acowork_core::Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    // ── Step 8: Create chunk channel ──────────────────────────────────
    // ADR-021: Single channel for control events only.
    // Data events (Delta, ReasoningDelta, ToolCall, ToolResult) are no longer
    // pushed via channel — the frontend polls them via HTTP.
    let (chunk_tx, chunk_rx) = if mqtt_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::SessionChunkEvent>(
            config.data_flow.chunk_capacity,
        );
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // ── Capture reconnect params for Gateway mode ───────────────────
    let agent_id = config.agent_id.clone();

    Ok(AgentBootContext {
        loaded,
        mqtt_client,
        available_cache,
        control_rx,
        runtime_http_port,
        provider,
        resolved_model,
        available_models,
        protocol_type,
        gateway_current_provider_id,
        emb_provider,
        active_tools,
        tool_definitions,
        full_tool_specs,
        system_prompt,
        memory_session,
        mcp_notifier,
        workspace_resolver,
        context_builder: Some(context_builder),
        identity_context,
        chunk_tx,
        chunk_rx,
        budget,
        resource_cache,
        session_snapshots,
        latest_session,
        agent_id,
        http_dispatch_rx: Some(http_dispatch_rx),
        // ADR-033: shared memory store + embed dim are populated by Phase B
        // and read by the Runtime HTTP memory handlers. The HTTP server was
        // already given clones of these Arcs in the `start(...)` call above.
        memory_store_shared,
        embed_dim_shared,
        degraded_reasons,
        agent_core_shared,
        session_metadata_slot,
        memory_query_slot,
        workspace_query_slot,
        workspace_mutation_slot,
    })
}

/// Return a noop provider tuple for fallback cases where no provider list
/// is available from Gateway or MQTT cache.
fn noop_provider_tuple(
) -> (
    std::sync::Arc<dyn acowork_core::providers::traits::Provider>,
    String,
    Vec<String>,
    ProtocolType,
) {
    (
        crate::providers::router::create_noop_provider(),
        "no-model".to_string(),
        vec![],
        ProtocolType::OpenAI,
    )
}
