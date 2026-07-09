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

use crate::gateway::state::GatewayState;
use crate::grpc::SharedGrpcSessionMgr;
use crate::http::routes::SharedHttpState;
use acowork_core::protocol::{GatewayResponse, SidecarKind};

/// Unified pusher for global resource changes (provider/model, MCP catalog, …).
#[derive(Clone)]
pub struct GlobalResourcePusher {
    grpc_session_mgr: Option<SharedGrpcSessionMgr>,
    gateway_state: SharedHttpState,
    data_dir: PathBuf,
}

/// Build the (endpoint, spec_json) payload for a `SidecarEndpointUpdate`
/// targeting `SidecarKind::Embed`, derived from the current GatewayState.
///
/// Returns `None` when no embed process is running or when the active
/// model has not yet been resolved — callers should treat `None` as
/// "nothing to push" and skip the call rather than push an empty payload.
///
/// This helper is `pub(crate)` so the embed supervisor and embedding HTTP
/// API can share the same payload construction logic without duplicating
/// it. Introduced in ADR-030 Phase C2.
pub(crate) fn build_embed_sidecar_payload(state: &GatewayState) -> Option<(String, String)> {
    let eps = state.embed_process.as_ref()?;
    let model_id = eps.active_model_id.as_ref()?;
    let endpoint = format!("http://127.0.0.1:{}/v1", eps.port);
    let spec_json = serde_json::json!({
        "model_id": model_id,
        "dimension": eps.active_dimension.unwrap_or(0),
    })
    .to_string();
    Some((endpoint, spec_json))
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
                        avatar: None,
                        builtin_avatar: None,
                        max_sessions: None,
                        context_window: None,
                        approval_timeout_secs: None,
                        builtin_tools_enabled: None,
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

    // ── Sidecar endpoint (embed / lsp_relay / …) ────────────────────────

    /// Push a sidecar endpoint update to all running agents.
    ///
    /// This is the canonical channel for sidecar state changes (embed model
    /// switched, lsp_relay ready, sidecar crash, …). Both sidecars share a
    /// single wire message (`GatewayResponse::SidecarEndpointUpdate`); the
    /// Runtime uses [`SidecarKind`] to route the payload to the correct
    /// subsystem.
    ///
    /// An empty `endpoint` signals "sidecar is unavailable" — the Runtime
    /// should disable dependent features rather than try to connect. This
    /// matches the protocol convention defined in ADR-030 C1.2.
    ///
    /// Introduced in ADR-030 Phase C2. As of C4, this is the only channel
    /// for sidecar state pushes; both `embed` and `lsp_relay` use it.
    #[tracing::instrument(skip(self), name = "push_sidecar_endpoint")]
    pub async fn push_sidecar_endpoint(
        &self,
        sidecar: SidecarKind,
        endpoint: String,
        spec_json: String,
    ) {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!(
                    sidecar = %sidecar.as_str(),
                    "No gRPC session manager, skipping sidecar push"
                );
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

        let mut pushed = 0u32;
        let mut failed = 0u32;

        for agent_id in agent_ids {
            let mgr = grpc_session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                let ok = session
                    .push_message(GatewayResponse::SidecarEndpointUpdate {
                        sidecar,
                        endpoint: endpoint.clone(),
                        spec_json: spec_json.clone(),
                    })
                    .await;

                if ok {
                    tracing::info!(
                        agent = %agent_id,
                        sidecar = %sidecar.as_str(),
                        endpoint = %endpoint,
                        "Pushed sidecar endpoint to agent"
                    );
                    pushed += 1;
                } else {
                    tracing::warn!(
                        agent = %agent_id,
                        sidecar = %sidecar.as_str(),
                        "Sidecar push failed (channel closed)"
                    );
                    failed += 1;
                }
            }
        }

        if pushed > 0 || failed > 0 {
            tracing::info!(
                sidecar = %sidecar.as_str(),
                pushed,
                failed,
                "Sidecar push complete"
            );
        }
    }

    /// Push a `MigrationStart` command to a single agent by ID.
    ///
    /// Returns true if the message was successfully queued for delivery.
    /// Used by the embedding migration HTTP endpoint to start per-agent
    /// dimension migration.
    pub async fn push_migration_start(
        &self,
        agent_id: &str,
        request_id: &str,
        embed_endpoint: &str,
        embed_model_id: &str,
        embed_dimension: usize,
    ) -> bool {
        let grpc_session_mgr = match &self.grpc_session_mgr {
            Some(mgr) => mgr.clone(),
            None => {
                tracing::warn!("No gRPC session manager, cannot push MigrationStart");
                return false;
            }
        };

        let mgr = grpc_session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(agent_id) {
            let msg = GatewayResponse::MigrationStart {
                request_id: request_id.to_string(),
                embed_endpoint: embed_endpoint.to_string(),
                embed_model_id: embed_model_id.to_string(),
                embed_dimension,
            };
            session.push_message(msg).await
        } else {
            tracing::warn!(agent = %agent_id, "Agent not found in gRPC session manager");
            false
        }
    }

    /// Check if the GlobalResourcePusher has a usable gRPC session manager.
    pub fn has_grpc_mgr(&self) -> bool {
        self.grpc_session_mgr.is_some()
    }
}
