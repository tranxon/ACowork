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

use crate::http::models_api::ModelsCache;
use crate::http::routes::{SharedHttpState, SharedSessionMgr};
use rollball_core::protocol::GatewayResponse;

/// Unified pusher for global resource changes (provider/model, MCP catalog, …).
#[derive(Clone)]
pub struct GlobalResourcePusher {
    session_mgr: Option<SharedSessionMgr>,
    gateway_state: SharedHttpState,
    data_dir: PathBuf,
    models_cache: ModelsCache,
}

impl GlobalResourcePusher {
    #[allow(dead_code)]
    pub(crate) fn new(
        session_mgr: Option<SharedSessionMgr>,
        gateway_state: SharedHttpState,
        data_dir: PathBuf,
        models_cache: ModelsCache,
    ) -> Self {
        Self { session_mgr, gateway_state, data_dir, models_cache }
    }

    // ── LLM config (provider/model change) ──────────────────────────

    /// Push LLM configuration to all running agents after a Vault change
    /// (add/update/delete provider key).
    #[tracing::instrument(skip(self), name = "push_llm_config")]
    pub async fn push_llm_config(&self) {
        use crate::ipc::server::resolve_llm_config_for_agent;

        let session_mgr = match &self.session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No IPC session manager, skipping LLM config push");
                return;
            }
        };

        let agent_ids: Vec<String> = {
            let gw = self.gateway_state.read().await;
            gw.running_agents.keys().cloned().collect()
        };

        for agent_id in agent_ids {
            if let Some(cfg) =
                resolve_llm_config_for_agent(&agent_id, &self.gateway_state).await
            {
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

                let mgr = session_mgr.lock().await;
                if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                    let max_output_tokens_limit = self
                        .gateway_state
                        .read()
                        .await
                        .config
                        .as_ref()
                        .map(|c| c.max_output_tokens_limit)
                        .unwrap_or(32_768);

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

    // ── MCP catalog ─────────────────────────────────────────────────

    /// Push search config changes to all running agents after a vault mutation.
    #[tracing::instrument(skip(self), name = "push_search_config")]
    pub async fn push_search_config(&self) {
        let session_mgr = match &self.session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No IPC session manager, skipping search config push");
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
            let mgr = session_mgr.lock().await;
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
        use tokio::task::JoinSet;

        let session_mgr = match &self.session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No IPC session manager, skipping MCP catalog push");
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

        // ── Phase 3: Clone senders under brief lock ──
        let senders: Vec<(String, crate::ipc::session::PushSender)> = {
            let mgr = session_mgr.lock().await;
            push_targets
                .iter()
                .filter_map(|(aid, _)| {
                    mgr.find_by_agent_id(aid)
                        .and_then(|(_, session)| session.push_sender().cloned())
                        .map(|tx| (aid.clone(), tx))
                })
                .collect()
        };

        if senders.is_empty() {
            return;
        }

        let payload_map: std::collections::HashMap<String, Vec<McpServerConfigDef>> =
            push_targets.into_iter().collect();

        // ── Phase 4: Concurrent push (lock-free) ──
        let mut join_set = JoinSet::new();
        for (aid, sender) in senders {
            let merged = match payload_map.get(&aid) {
                Some(s) => s.clone(),
                None => continue,
            };
            join_set.spawn(async move {
                let result = sender
                    .send(GatewayResponse::RuntimeConfigUpdate {
                        mcp_servers: Some(merged),
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
                (aid, result.is_ok())
            });
        }

        let mut pushed = 0u32;
        let mut failed = 0u32;
        while let Some(res) = join_set.join_next().await {
            match res {
                Ok((aid, true)) => {
                    tracing::info!(agent = %aid, "Pushed MCP config to agent");
                    pushed += 1;
                }
                Ok((aid, false)) => {
                    tracing::warn!(agent = %aid, "MCP config push failed (channel closed)");
                    failed += 1;
                }
                Err(e) => {
                    tracing::warn!("MCP config push task panicked: {}", e);
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
        let session_mgr = match &self.session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No IPC session manager, skipping user profile push");
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
            let mgr = session_mgr.lock().await;
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
