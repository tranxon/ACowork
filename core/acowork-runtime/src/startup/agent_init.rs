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
        RuntimeResourceCache, connect_gateway_client, read_resource_cache, save_resource_cache,
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

    // ── Step 2: Connect to Gateway gRPC ─────────────────────────────
    let mut grpc_client: Option<crate::grpc::client::GatewayGrpcClient> = None;
    let mut hello_config: Option<crate::grpc::client::AgentHelloConfig> = None;
    if let Some(endpoint) = config.get_gateway_address()
        && let Some((client, cfg)) = connect_gateway_client(
            endpoint,
            &loaded.manifest.agent_id,
            &loaded.manifest.version,
            &config.work_dir,
            config.data_flow.outbound_ctrl_capacity,
        )
        .await
    {
        // Persist resource versions + lists for next startup's diff sync.
        let prov_list = cfg.provider_list.clone();
        let mcp_list_data = cfg.mcp_list.clone();
        let prov_ver = cfg.provider_list_version;
        let mcp_ver = cfg.mcp_list_version;
        let search_ver = cfg.search_list_version;
        let old_cache = read_resource_cache(std::path::Path::new(&config.work_dir));
        let new_cache = RuntimeResourceCache {
            provider_list_version: prov_ver,
            mcp_list_version: mcp_ver,
            search_list_version: search_ver,
            user_profile_version: cfg.user_profile_version,
            providers: prov_list.or(old_cache.providers),
            mcps: mcp_list_data.or(old_cache.mcps),
        };
        grpc_client = Some(client);
        hello_config = Some(cfg);
        save_resource_cache(std::path::Path::new(&config.work_dir), &new_cache);
    }
    if grpc_client.is_some() {
        tracing::info!("Gateway gRPC client initialized");

        // ── Send workspace config snapshot immediately after AgentHello ──
        // Sending here (Phase A) instead of Phase C ensures the Gateway's
        // in-memory cache is populated before any frontend workspace-list
        // queries. Previously the window between AgentHello (Phase A) and
        // UpdateWorkspaceConfig (Phase C) caused a race: the frontend could
        // query GET /workspaces and receive an empty list.
        if let Some(ref mut client) = grpc_client {
            let work_dir_path = std::path::Path::new(&config.work_dir);
            let ws_config_path = work_dir_path.join("config").join("agent_workspaces.json");
            let ws_config_json = if ws_config_path.exists() {
                std::fs::read_to_string(&ws_config_path)
                    .unwrap_or_else(|_| r#"{"version":"1.0.0","additional_dirs":[]}"#.to_string())
            } else {
                r#"{"version":"1.0.0","additional_dirs":[]}"#.to_string()
            };
            let msg = acowork_core::proto::ClientMessage {
                request_id: 0,
                payload: Some(
                    acowork_core::proto::client_message::Payload::UpdateWorkspaceConfig(
                        acowork_core::proto::UpdateWorkspaceConfig { config_json: ws_config_json },
                    ),
                ),
            };
            if client.outbound_ctrl_sender().send(msg).await.is_err() {
                tracing::warn!("Failed to send UpdateWorkspaceConfig snapshot to Gateway (Phase A)");
            } else {
                tracing::info!("Workspace config snapshot sent to Gateway (Phase A)");
            }
        }
    } else {
        tracing::info!("Running in standalone mode (no Gateway)");
    }

    // ── Step 3: Build system prompt ─────────────────────────────────
    let skill_mode =
        crate::startup::super_mod::resolve_skill_mode(&loaded.manifest, &config.work_dir);
    let system_prompt = build_system_prompt_with_mode(&loaded.package_dir, skill_mode)?;
    tracing::debug!(prompt_len = system_prompt.len(), "System prompt built");

    // ── Step 3.5: Load skill registry ───────────────────────────────
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

    // ── Step 3: Initialize LLM Provider ─────────────────────────────
    let mut gateway_current_provider_id: Option<String> = None;
    let resource_cache = read_resource_cache(std::path::Path::new(&config.work_dir));

    let (provider, resolved_model, available_models, protocol_type) = {
        if let Some(ref cfg) = hello_config {
            let provider_list = cfg
                .provider_list
                .as_ref()
                .or(resource_cache.providers.as_ref());
            if let Some(providers) = provider_list {
                let has_api_key = |prov_id: &str| -> bool {
                    cfg.provider_key_vault
                        .iter()
                        .any(|k| k.provider_id == prov_id)
                };
                let chosen_prov = providers.iter().find(|p| has_api_key(&p.id));
                if let Some(prov) = chosen_prov {
                    gateway_current_provider_id = Some(prov.id.clone());
                    let api_key = cfg
                        .provider_key_vault
                        .iter()
                        .find(|k| k.provider_id == prov.id)
                        .map(|k| k.api_key.as_str());
                    let available = prov.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
                    let model_id = prov
                        .models
                        .first()
                        .map(|m| m.id.clone())
                        .unwrap_or_else(|| "default".to_string());
                    let timeouts = Some(crate::providers::router::ProviderTimeouts::from(config));
                    let provider = crate::providers::router::create_provider(
                        &prov.id,
                        &prov.protocol_type,
                        api_key,
                        Some(&prov.base_url),
                        timeouts,
                    );
                    tracing::info!(
                        provider = %prov.id,
                        model = %model_id,
                        num_models = available.len(),
                        has_api_key = api_key.is_some(),
                        source = "manifest",
                        "Provider initialized from AgentHelloConfig"
                    );
                    (provider, model_id, available, prov.protocol_type.clone())
                } else {
                    tracing::warn!(
                        available = ?providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
                        "No provider with API key found, using noop"
                    );
                    let p = crate::providers::router::create_noop_provider();
                    (p, "no-model".to_string(), vec![], ProtocolType::OpenAI)
                }
            } else {
                tracing::warn!("No provider list available from Gateway or cache, using noop");
                let p = crate::providers::router::create_noop_provider();
                (p, "no-model".to_string(), vec![], ProtocolType::OpenAI)
            }
        } else {
            let p = crate::providers::router::create_noop_provider();
            (p, "no-model".to_string(), vec![], ProtocolType::OpenAI)
        }
    };

    // ── Step 3.5: Build Embedding Provider (single-tier, no fallback) ──
    //
    // ADR: 移除 FallbackEmbeddingProvider 三层链。
    //
    // 唯一来源：Gateway AgentHelloConfig.embed_endpoint/model_id/dimension
    // （即 embed_supervisor 管理的本地 ONNX 服务，端口 18080）。
    //
    // 失败语义：
    //   - endpoint 存在 + provider 构造成功 → 打印 INFO ✅ 继续
    //   - endpoint 存在 + provider 构造失败 → 打印 ERROR ❌ + emb_provider = None
    //   - endpoint 不存在（standalone / Gateway embed_supervisor 未启动）
    //     → 打印 ERROR ❌ + emb_provider = None
    //
    // 失败时 emb_provider = None，runtime 继续启动。
    // memory::manager 会在 query.embedding.is_none() 时自动 fallback 到
    // text_search_with_filter（manager.rs L270-280），不会让 memory 静默失效。
    // 用户重启 runtime 时从启动日志就能立刻判断第 1 层 ONNX 是否通。

    let embed_endpoint = hello_config
        .as_ref()
        .and_then(|cfg| cfg.embed_endpoint.clone())
        .or_else(|| std::env::var("ACOWORK_EMBED_ENDPOINT").ok());
    let embed_model_id = hello_config
        .as_ref()
        .and_then(|cfg| cfg.embed_model_id.clone())
        .or_else(|| std::env::var("ACOWORK_EMBED_MODEL").ok())
        .unwrap_or_else(|| "bge-small-zh-v1.5".to_string());
    let embed_dimension = hello_config
        .as_ref()
        .and_then(|cfg| cfg.embed_dimension)
        .or_else(|| {
            std::env::var("ACOWORK_EMBED_DIMENSION")
                .ok()
                .and_then(|s| s.parse().ok())
        })
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
                "❌ No embed_endpoint in AgentHelloConfig and ACOWORK_EMBED_ENDPOINT not set. \
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
    let has_search_providers = hello_config
        .as_ref()
        .map(|c| !c.search_key_vault.is_empty())
        .unwrap_or(false);

    let lsp_relay_endpoint = hello_config
        .as_ref()
        .and_then(|c| c.lsp_relay_endpoint.clone());

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
    ) {
        registry.register(tool);
    }

    let active_tools = registry.activate(&loaded.manifest, &workspace_resolver, 60, &[]);
    tracing::info!(
        total = registry.all().len(),
        active = active_tools.len(),
        enabled = active_tools.iter().filter(|e| e.enabled).count(),
        "Tools activated (initial — no agent_tools.json yet; will be re-merged below)"
    );

    // ── ADR-029 Step B: Resolve `agent_tools.json` enabled flags ────
    //
    // 1. If `{work_dir}/config/agent_tools.json` exists → load it
    // 2. If absent → generate initial config from manifest `[[tools]]`
    //    (or enable-all when manifest has no `[[tools]]`)
    // 3. Merge code-registered tools with persisted entries to handle
    //    upgrades (new tools default to enabled=false; removed tools
    //    silently dropped)
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
                crate::agent_config::merge_tools_config(&code_tool_list, &persisted_cfg.tools)
            }
            Ok(None) => {
                let initial = if loaded.manifest.has_any_tool_declaration() {
                    tracing::info!(
                        manifest_tools = manifest_tool_names.len(),
                        "agent_tools.json absent — seeding from manifest.toml [[tools]]"
                    );
                    crate::agent_config::init_tools_config_from_manifest(
                        &code_tool_list,
                        &manifest_tool_names,
                    )
                } else {
                    tracing::info!(
                        "agent_tools.json absent and manifest has no [[tools]] — all builtin tools enabled by default"
                    );
                    crate::agent_config::all_enabled_tools_config(&code_tool_list)
                };
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

    // Now re-activate with the merged enabled list. We use a fresh
    // registry walk so each tool gets the post-merge `enabled` flag.
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
    let identity_context: Option<String> = hello_config
        .as_ref()
        .and_then(|cfg| cfg.user_identity.as_ref())
        .map(crate::agent::session::session_manager::format_user_profile_context);

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
    let (chunk_tx, chunk_rx) = if grpc_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::SessionChunkEvent>(
            config.data_flow.chunk_capacity,
        );
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // ── Capture reconnect params for Gateway mode ───────────────────
    let agent_id = config.agent_id.clone();
    let version = loaded.manifest.version.clone();
    let socket_path = config
        .get_gateway_address()
        .map(|s| s.to_string())
        .unwrap_or_default();

    Ok(AgentBootContext {
        loaded,
        grpc_client,
        hello_config,
        provider,
        resolved_model,
        available_models,
        protocol_type,
        gateway_current_provider_id,
        emb_provider,
        active_tools,
        tool_definitions,
        full_tool_specs,
        skill_registry,
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
        agent_id,
        version,
        socket_path,
    })
}
