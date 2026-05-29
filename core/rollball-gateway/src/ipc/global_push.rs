//! Unified global resource pusher.
//!
//! Replaces the ad-hoc `hot_push_llm_config` (vault_api.rs) and
//! `hot_push_mcp_config` (mcp_catalog_api.rs) functions with a single
//! struct. All HTTP handlers call `push_llm_config()` or
//! `push_mcp_catalog()` after mutating global state.
//!
//! ## Adding a new resource type
//!
//! 1. Add a `pub async fn push_<resource>(&self)` method
//! 2. Call it from the HTTP handler that mutates that resource
//!
//! The push pipeline (collect running agents → build payloads →
//! concurrent push via JoinSet → log results) is shared.

use std::path::PathBuf;

use crate::grpc::SharedGrpcSessionMgr;
use crate::http::models_api::ModelsCache;
use crate::http::routes::SharedHttpState;
use rollball_core::protocol::GatewayResponse;

/// Unified pusher for global resource changes (provider/model, MCP catalog, …).
#[derive(Clone)]
pub struct GlobalResourcePusher {
    grpc_session_mgr: Option<SharedGrpcSessionMgr>,
    gateway_state: SharedHttpState,
    data_dir: PathBuf,
    models_cache: ModelsCache,
}

impl GlobalResourcePusher {
    #[allow(dead_code)]
    pub(crate) fn new(
        grpc_session_mgr: Option<SharedGrpcSessionMgr>,
        gateway_state: SharedHttpState,
        data_dir: PathBuf,
        models_cache: ModelsCache,
    ) -> Self {
        Self { grpc_session_mgr, gateway_state, data_dir, models_cache }
    }

    // ── LLM config (provider/model change) ──────────────────────────

    /// Push LLM configuration to all running agents after a Vault change
    /// (add/update/delete provider key).
    ///
    /// Pushes config for ALL vault providers (not just the default one)
    /// so that per-provider fields like compact_model are updated for
    /// every provider, not just the active one.
    #[tracing::instrument(skip(self), name = "push_llm_config")]
    pub async fn push_llm_config(&self) {

        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping LLM config push");
                return;
            }
        };

        let (agent_ids, provider_ids): (Vec<String>, Vec<String>) = {
            let gw = self.gateway_state.read().await;
            (
                gw.running_agents.keys().cloned().collect(),
                gw.vault.list_providers(),
            )
        };

        for agent_id in &agent_ids {
            for provider_id in &provider_ids {
                let cfg = match self
                    .build_llm_config_for_provider(provider_id)
                    .await
                {
                    Some(c) => c,
                    None => continue,
                };

                let model_capabilities = if cfg.stored_capabilities.is_some() {
                    cfg.stored_capabilities
                } else if let Some(ref m) = cfg.model {
                    crate::http::models_api::lookup_model_capabilities_with_cache(
                        &self.models_cache, &cfg.provider, m,
                    )
                    .await
                } else {
                    None
                };

                let (protocol_type, api_override) =
                    crate::http::models_api::lookup_protocol_info_with_cache(
                        &self.models_cache, &cfg.provider, cfg.model.as_deref(),
                    )
                    .await;

                let effective_base_url = api_override.or(cfg.base_url.clone());

                let max_output_tokens_limit = self
                    .gateway_state
                    .read()
                    .await
                    .config
                    .as_ref()
                    .map(|c| c.max_output_tokens_limit)
                    .unwrap_or(32_768);

                let provider_list_version = self
                    .gateway_state
                    .read()
                    .await
                    .resource_cache
                    .provider_list
                    .version;

                let mgr = grpc_session_mgr.lock().await;
                if let Some((_conn_id, session)) = mgr.find_by_agent_id(agent_id) {
                    let ok = session
                        .push_message(GatewayResponse::LLMConfigDelivery {
                            provider: cfg.provider.clone(),
                            model: cfg.model.clone(),
                            api_key: cfg.api_key.clone(),
                            base_url: effective_base_url,
                            models: cfg.models.clone(),
                            model_capabilities,
                            max_output_tokens_limit,
                            protocol_type,
                            compact_model: cfg.compact_model.clone(),
                            provider_list_version,
                        })
                        .await;

                    if ok {
                        tracing::info!(
                            agent = %agent_id,
                            provider = %cfg.provider,
                            "Pushed LLM config to agent"
                        );
                    } else {
                        tracing::warn!(
                            agent = %agent_id,
                            "LLM config push failed (channel closed)"
                        );
                    }
                }
            }
        }
    }

    /// Build a ResolvedLlmConfig for a specific provider (not just the default).
    async fn build_llm_config_for_provider(
        &self,
        provider_id: &str,
    ) -> Option<crate::ipc::server::ResolvedLlmConfig> {
        let gw = self.gateway_state.read().await;
        let entry = match gw.vault.get_provider(provider_id) {
            Ok(e) => e,
            Err(_) => return None,
        };

        Some(crate::ipc::server::ResolvedLlmConfig {
            provider: provider_id.to_string(),
            model: entry.default_model.clone(),
            api_key: entry.api_key.clone(),
            base_url: entry.base_url.clone(),
            models: entry.models.clone(),
            stored_capabilities: entry
                .default_model
                .as_ref()
                .and_then(|m| entry.model_capabilities.get(m))
                .map(|c| rollball_core::protocol::ModelCapabilitiesInfo::from(c.clone())),
            compact_model: entry.compact_model.clone(),
        })
    }

    // ── MCP catalog ─────────────────────────────────────────────────

    /// Push search config changes to all running agents after a vault mutation.
    #[tracing::instrument(skip(self), name = "push_search_config")]
    pub async fn push_search_config(&self) {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping search config push");
                return;
            }
        };

        let agent_ids: Vec<String> = {
            let gw = self.gateway_state.read().await;
            gw.running_agents.keys().cloned().collect()
        };

        if agent_ids.is_empty() {
            return;
        }

        // Build search config payload from Gateway resource cache + vault
        let (search_list, search_list_version, search_key_vault) = {
            let gw = self.gateway_state.read().await;
            let list = gw.resource_cache.search_list.providers.clone();
            let version = gw.resource_cache.search_list.version;
            let keys = crate::resource_cache::build_search_key_vault(&gw);
            (list, version, keys)
        };

        for agent_id in agent_ids {
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                let ok = session
                    .push_message(GatewayResponse::SearchConfigDelivery {
                        search_list: search_list.clone(),
                        search_list_version,
                        search_key_vault: search_key_vault.clone(),
                    })
                    .await;

                if ok {
                    tracing::info!(agent = %agent_id, "Pushed search config to agent");
                } else {
                    tracing::warn!(agent = %agent_id, "Search config push failed (channel closed)");
                }
            }
        }
    }

    // ── MCP catalog ─────────────────────────────────────────────────

    /// Push MCP catalog changes to all running agents after a catalog mutation.
    #[tracing::instrument(skip(self), name = "push_mcp_catalog")]
    pub async fn push_mcp_catalog(&self) {
        use crate::http::mcp_catalog_api;
        use rollball_core::protocol::McpServerConfigDef;

        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping MCP catalog push");
                return;
            }
        };

        // ── Phase 1: Collect running agent IDs ──
        let agent_ids: Vec<String> = {
            let gw = self.gateway_state.read().await;
            gw.running_agents.keys().cloned().collect()
        };

        // Load catalog once
        let catalog = match mcp_catalog_api::load_mcp_catalog(&self.data_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to load MCP catalog for push: {}", e);
                return;
            }
        };

        // Per-agent MCP config is now owned by Runtime ({work_dir}/config/agent_config.json).
        // Gateway no longer stores per-agent MCP configuration, so we cannot filter
        // by per-agent active servers. Push the full catalog to all running agents;
        // Runtime will filter based on its own persisted config.
        if agent_ids.is_empty() || catalog.is_empty() {
            return;
        }

        let push_targets: Vec<(String, Vec<McpServerConfigDef>)> =
            agent_ids.into_iter().map(|aid| (aid, catalog.clone())).collect();

        if push_targets.is_empty() {
            return;
        }

        // Phase 3: Push to all running agents via gRPC
        let mut pushed = 0u32;
        let mut failed = 0u32;
        for aid in push_targets.iter().map(|(aid, _)| aid.clone()) {
            let servers = match push_targets.iter().find_map(|(a, s)| if a == &aid { Some(s.clone()) } else { None }) {
                Some(s) => s,
                None => continue,
            };
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&aid) {
                let ok = session
                    .push_message(GatewayResponse::RuntimeConfigUpdate {
                        mcp_servers: Some(servers),
                        max_output_tokens: None,
                        max_iterations: None,
                        temperature: None,
                        system_prompt_override: None,
                        active_tools: None,
                        shell_approval_threshold: None,
                        model: None,
                        provider: None,
                        search_config_json: None,
                    })
                    .await;
                if ok {
                    tracing::info!(agent = %aid, "Pushed MCP config to agent");
                    pushed += 1;
                } else {
                    tracing::warn!(agent = %aid, "MCP config push failed (channel closed)");
                    failed += 1;
                }
            }
        }

        if pushed > 0 || failed > 0 {
            tracing::info!(pushed, failed, "MCP catalog push complete");
        }
    }

    // ── User profile ────────────────────────────────────────────────

    /// Push active user profile to all running agents after a profile change.
    #[tracing::instrument(skip(self), name = "push_user_profile")]
    pub async fn push_user_profile(&self) {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping user profile push");
                return;
            }
        };

        let agent_ids: Vec<String> = {
            let gw = self.gateway_state.read().await;
            gw.running_agents.keys().cloned().collect()
        };

        if agent_ids.is_empty() {
            return;
        }

        let (user_identity, version) = {
            let gw = self.gateway_state.read().await;
            let active_user = gw.resource_cache.user_profile_list.users
                .iter()
                .find(|u| u.is_active)
                .cloned();
            (active_user, gw.resource_cache.user_profile_list.version)
        };

        for agent_id in agent_ids {
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                let ok = session
                    .push_message(GatewayResponse::UserProfileUpdate {
                        user_identity: user_identity.clone(),
                        version,
                    })
                    .await;

                if ok {
                    tracing::info!(agent = %agent_id, version, "Pushed user profile to agent");
                } else {
                    tracing::warn!(agent = %agent_id, "User profile push failed (channel closed)");
                }
            }
        }
    }
}
