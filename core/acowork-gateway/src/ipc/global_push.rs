//! Unified global resource pusher.
//!
//! Replaces the ad-hoc `hot_push_llm_config` (provider_api.rs) and
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
use crate::http::routes::SharedHttpState;
use acowork_core::protocol::GatewayResponse;

/// Unified pusher for global resource changes (provider/model, MCP catalog, …).
#[derive(Clone)]
pub struct GlobalResourcePusher {
    grpc_session_mgr: Option<SharedGrpcSessionMgr>,
    gateway_state: SharedHttpState,
    data_dir: PathBuf,
}

impl GlobalResourcePusher {
    #[allow(dead_code)]
    pub(crate) fn new(
        grpc_session_mgr: Option<SharedGrpcSessionMgr>,
        gateway_state: SharedHttpState,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            grpc_session_mgr,
            gateway_state,
            data_dir,
        }
    }

    // ── Provider list (provider/model/key change) ───────────────────

    /// Push the full provider list to all running agents after a Vault or
    /// provider-cache change (add/update/delete provider, key rotation, etc.).
    ///
    /// Sends a single `ProviderListUpdate` per agent — identical payload to
    /// what `AgentHelloResult` delivers on handshake — so the Runtime can
    /// rebuild its provider registry atomically without any per-provider
    /// fan-out. The cache is the source of truth: HTTP handlers that mutate
    /// the Vault must rebuild it (see `provider_api.rs`) before calling this.
    #[tracing::instrument(skip(self), name = "push_llm_config")]
    pub async fn push_llm_config(&self) {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping provider list push");
                return;
            }
        };

        // Snapshot agent list + provider list + key vault in a single read lock.
        let (agent_ids, provider_list, provider_list_version, provider_key_vault) = {
            let gw = self.gateway_state.read().await;
            let agent_ids: Vec<String> = gw.running_agents.keys().cloned().collect();
            let provider_list = gw.resource_cache.provider_list.providers.clone();
            let provider_list_version = gw.resource_cache.provider_list.version;
            let provider_key_vault: Vec<acowork_core::protocol::ProviderKeyEntry> = gw
                .vault
                .list_providers()
                .iter()
                .filter_map(|name| {
                    gw.vault.get_provider(name).ok().map(|entry| {
                        acowork_core::protocol::ProviderKeyEntry {
                            provider_id: name.clone(),
                            api_key: entry.api_key,
                        }
                    })
                })
                .collect();
            (
                agent_ids,
                provider_list,
                provider_list_version,
                provider_key_vault,
            )
        };

        if agent_ids.is_empty() {
            return;
        }

        for agent_id in &agent_ids {
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(agent_id) {
                let ok = session
                    .push_message(GatewayResponse::ProviderListUpdate {
                        provider_list: provider_list.clone(),
                        provider_list_version,
                        provider_key_vault: provider_key_vault.clone(),
                    })
                    .await;

                if ok {
                    tracing::info!(
                        agent = %agent_id,
                        providers = provider_list.len(),
                        version = provider_list_version,
                        "Pushed provider list to agent"
                    );
                } else {
                    tracing::warn!(
                        agent = %agent_id,
                        "Provider list push failed (channel closed)"
                    );
                }
            }
        }
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
        use acowork_core::protocol::McpServerConfigDef;

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
        //
        // NOTE: Do NOT skip when catalog is empty — the Runtime needs to receive
        // an empty catalog push to clear its agent_mcp.json.  Skipping here would
        // leave stale catalog entries in per-agent config after all MCPs are removed.
        if agent_ids.is_empty() {
            return;
        }

        let push_targets: Vec<(String, Vec<McpServerConfigDef>)> = agent_ids
            .into_iter()
            .map(|aid| (aid, catalog.clone()))
            .collect();

        if push_targets.is_empty() {
            return;
        }

        // Phase 3: Push to all running agents via gRPC
        let mut pushed = 0u32;
        let mut failed = 0u32;
        for aid in push_targets.iter().map(|(aid, _)| aid.clone()) {
            let servers = match push_targets
                .iter()
                .find_map(|(a, s)| if a == &aid { Some(s.clone()) } else { None })
            {
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
                        shell_approval_threshold: None,
                        model: None,
                        provider: None,
                        search_config_json: None,
                        embed_config_json: {
                            let gw = self.gateway_state.read().await;
                            match &gw.embed_process {
                                Some(eps) if eps.active_model_id.is_some() => {
                                    Some(serde_json::json!({
                                        "embed_endpoint": format!("http://127.0.0.1:{}/v1", eps.port),
                                        "embed_model_id": eps.active_model_id.clone().unwrap_or_default(),
                                        "embed_dimension": eps.active_dimension.unwrap_or(0),
                                    }).to_string())
                                }
                                _ => None,
                            }
                        },
                        avatar: None,
                        builtin_avatar: None,
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
            let active_user = gw
                .resource_cache
                .user_profile_list
                .users
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

    // ── Embedding config ────────────────────────────────────────────────

    /// Push embedding configuration update to all running agents after a
    /// model switch. The Runtime rebuilds its FallbackEmbeddingProvider
    /// chain with the new ONNX provider as the first entry.
    ///
    /// Uses `RuntimeConfigUpdate.embed_config_json` instead of the deprecated
    /// `EmbeddingConfigUpdate` variant, because the latter has no proto
    /// representation and would be lost over the gRPC bridge.
    #[tracing::instrument(skip(self), name = "push_embedding_config")]
    pub async fn push_embedding_config(&self) {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, skipping embedding config push");
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

        // Read current embedding config from GatewayState
        let (embed_endpoint, embed_model_id, embed_dimension) = {
            let gw = self.gateway_state.read().await;
            match &gw.embed_process {
                Some(eps) => {
                    let endpoint = format!("http://127.0.0.1:{}/v1", eps.port);
                    let model_id = eps.active_model_id.clone().unwrap_or_default();
                    let dimension = eps.active_dimension.unwrap_or(0);
                    (endpoint, model_id, dimension)
                }
                None => {
                    tracing::warn!("Embedding service not running, skipping push");
                    return;
                }
            }
        };

        // Serialize as JSON for the embed_config_json field
        let embed_config_json = serde_json::json!({
            "embed_endpoint": embed_endpoint,
            "embed_model_id": embed_model_id,
            "embed_dimension": embed_dimension,
        })
        .to_string();

        let mut pushed = 0u32;
        let mut failed = 0u32;

        for agent_id in agent_ids {
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                let ok = session
                    .push_message(GatewayResponse::RuntimeConfigUpdate {
                        max_output_tokens: None,
                        max_iterations: None,
                        temperature: None,
                        system_prompt_override: None,
                        shell_approval_threshold: None,
                        mcp_servers: None,
                        model: None,
                        provider: None,
                        search_config_json: None,
                        embed_config_json: Some(embed_config_json.clone()),
                        avatar: None,
                        builtin_avatar: None,
                    })
                    .await;

                if ok {
                    tracing::info!(
                        agent = %agent_id,
                        model_id = %embed_model_id,
                        dimension = embed_dimension,
                        "Pushed embedding config to agent via RuntimeConfigUpdate"
                    );
                    pushed += 1;
                } else {
                    tracing::warn!(agent = %agent_id, "Embedding config push failed (channel closed)");
                    failed += 1;
                }
            }
        }

        if pushed > 0 || failed > 0 {
            tracing::info!(pushed, failed, "Embedding config push complete");
        }
    }
}
