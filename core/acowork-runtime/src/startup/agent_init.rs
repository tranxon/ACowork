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
use crate::error::{Result, RuntimeError};
use crate::startup::context::AgentBootContext;

/// Phase A: initialize all per-agent (cross-session) resources.
///
/// This function is the first phase of the agent startup sequence.
/// It produces an `AgentBootContext` that is consumed by subsequent phases.
pub(crate) async fn phase_a_init_agent(config: &RuntimeConfig) -> Result<AgentBootContext> {
    let _span = tracing::info_span!("startup_phase_a").entered();

    use crate::agent::context::ContextBuilder;
    use crate::embedding::ollama::OllamaEmbeddingProvider;
    use crate::embedding::remote::RemoteEmbeddingProvider;
    use crate::embedding::{EmbeddingConfig, EmbeddingProvider, FallbackEmbeddingProvider};
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt_with_mode;
    use crate::tools::builtin;
    use crate::tools::registry::ToolRegistry;
    use crate::startup::super_mod::{read_resource_cache, save_resource_cache, RuntimeResourceCache, connect_gateway_client};

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
    } else {
        tracing::info!("Running in standalone mode (no Gateway)");
    }

    // ── Step 3: Build system prompt ─────────────────────────────────
    let skill_mode = crate::startup::super_mod::resolve_skill_mode(&loaded.manifest, &config.work_dir);
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

    // ── Step 3.5: Build FallbackEmbeddingProvider ───────────────────
    let mut embedding_providers: Vec<(Box<dyn EmbeddingProvider>, u64)> = Vec::new();

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

    if let Some(ref endpoint) = embed_endpoint {
        match RemoteEmbeddingProvider::try_with_config(
            endpoint,
            None,
            &embed_model_id,
            embed_dimension,
        ) {
            Ok(ep) => {
                tracing::info!(
                    endpoint = %endpoint,
                    model = %embed_model_id,
                    dim = embed_dimension,
                    "ONNX embedding provider configured"
                );
                embedding_providers.push((Box::new(ep), 500));
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create ONNX embedding provider, skipping");
            }
        }
    }

    let ollama_primary = OllamaEmbeddingProvider::try_new().map_err(|e| {
        RuntimeError::Config(format!("Failed to create Ollama embedding provider: {e}"))
    })?;
    let ollama_dim = ollama_primary.dimension();
    embedding_providers.push((Box::new(ollama_primary), 200));

    let (base_url, api_key, model, dim) = {
        let providers = hello_config
            .as_ref()
            .and_then(|cfg| cfg.provider_list.as_deref())
            .unwrap_or(&[]);
        let key_vault = hello_config
            .as_ref()
            .map(|cfg| cfg.provider_key_vault.as_slice())
            .unwrap_or(&[]);

        let mut selected: Option<(String, Option<String>, String, usize)> = None;
        'outer: for p in providers {
            for m in &p.models {
                if m.capabilities.family.as_deref() == Some("text-embedding") {
                    let ak = key_vault
                        .iter()
                        .find(|k| k.provider_id == p.id)
                        .map(|k| k.api_key.clone());
                    let d = if m.capabilities.max_output_tokens > 0 {
                        m.capabilities.max_output_tokens as usize
                    } else {
                        ollama_dim
                    };
                    selected = Some((p.base_url.clone(), ak, m.id.clone(), d));
                    break 'outer;
                }
            }
        }
        match selected {
            Some((url, key, name, d)) => {
                tracing::info!(model = %name, dim = d, "Selected embedding model from provider list");
                (url, key, name, d)
            }
            None => {
                let url = providers
                    .first()
                    .map(|p| p.base_url.clone())
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
                let key = key_vault.first().map(|k| k.api_key.clone());
                tracing::info!(
                    "No embedding model found in provider list, defaulting to text-embedding-3-small"
                );
                (url, key, "text-embedding-3-small".to_string(), ollama_dim)
            }
        }
    };

    let remote_fallback =
        RemoteEmbeddingProvider::try_with_config(&base_url, api_key.as_deref(), &model, dim)
            .map_err(|e| {
                RuntimeError::Config(format!("Failed to create remote embedding provider: {e}"))
            })?;
    embedding_providers.push((Box::new(remote_fallback), 5000));

    let fallback_emb = FallbackEmbeddingProvider::with_providers(
        embedding_providers,
        EmbeddingConfig::default(),
    );
    // Lock the dimension to the first provider's dimension.
    // This prevents dimension mismatch when fallback providers have
    // different dimensions. The locked dimension must match the Grafeo
    // HNSW index dimension.
    let emb_dim = fallback_emb.dimension();
    let fallback_emb = fallback_emb.with_locked_dimension(emb_dim);
    let emb_provider: Arc<dyn EmbeddingProvider> = Arc::new(fallback_emb);
    tracing::info!(
        dim = emb_provider.dimension(),
        name = emb_provider.name(),
        "Embedding provider initialized"
    );

    // ── Step 4: Build tool registry + activate by manifest ──────────
    let workspace_resolver: crate::tools::workspace_resolver::SharedResolver =
        Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new(&config.work_dir),
        ));
    let has_search_providers = hello_config
        .as_ref()
        .map(|c| !c.search_key_vault.is_empty())
        .unwrap_or(false);

    let memory_session = Arc::new(crate::memory::MemorySessionHandle::new(Some(
        emb_provider.clone(),
    )));
    let mcp_notifier = Arc::new(crate::mcp_notify::McpConfigNotifier::default());

    let mut registry = ToolRegistry::new();
    for tool in builtin::all_builtin_tools(
        &workspace_resolver,
        &config.agent_id,
        config.tool_http_timeout_ms,
        has_search_providers,
        None,
        Some(memory_session.clone()),
        Some(mcp_notifier.clone()),
        config.work_dir.clone(),
    ) {
        registry.register(tool);
    }

    let active_tools = registry.activate(&loaded.manifest, &workspace_resolver, 60);
    tracing::info!(
        total = registry.all().len(),
        active = active_tools.len(),
        "Tools activated"
    );

    // ── Step 5: Build tool definitions ──────────────────────────────
    #[allow(unused_imports)]
    use acowork_core::tools::traits::Tool;
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
    let tool_definitions: Vec<serde_json::Value> =
        tool_specs.iter().map(|(_, v)| v.clone()).collect();

    let full_tool_specs: Vec<(String, serde_json::Value)> = registry
        .all()
        .iter()
        .map(|t| {
            let spec = t.spec();
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            (spec.name.clone(), serialized)
        })
        .collect();
    tracing::info!(
        active_specs = tool_specs.len(),
        full_specs = full_tool_specs.len(),
        "Tool specs: active vs full registry"
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

    // ── Step 8: Create chunk channels ───────────────────────────────
    // Data channel: high-capacity for streaming deltas (droppable under load)
    // Control channel: smaller, for control events that MUST reach frontend
    let (chunk_tx, chunk_rx) = if grpc_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::SessionChunkEvent>(256);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (control_chunk_tx, control_chunk_rx) = if grpc_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::SessionChunkEvent>(64);
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
        control_chunk_tx,
        control_chunk_rx,
        budget,
        resource_cache,
        agent_id,
        version,
        socket_path,
    })
}
