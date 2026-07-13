//! Gateway Service API handler implementations
//!
//! Contains handler functions for processing Gateway Service API requests.
//! These handlers are shared between the gRPC server (grpc/dispatch.rs)
//! and can be used by any transport layer.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::gateway::state::GatewayState;
use acowork_core::protocol::GatewayResponse;

/// Shared state type: Arc<RwLock<GatewayState>> for concurrent read/write access.
/// RwLock chosen because handlers are predominantly read-heavy (key lookup,
/// budget query) with occasional writes (install/uninstall).
pub type SharedState = Arc<RwLock<GatewayState>>;

// ── Handler implementations ─────────────────────────────────────────────────

pub async fn handle_key_release(
    provider: &str,
    agent_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    // Read-only access to GatewayState
    let state_guard = state.read().await;
    match state_guard.vault.get_key(provider) {
        Ok(api_key) => {
            tracing::info!("KeyRelease for agent={}, provider={}", agent_id, provider);
            GatewayResponse::KeyReleaseResult {
                api_key: Some(api_key),
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                "KeyRelease failed for agent={}, provider={}: {}",
                agent_id,
                provider,
                e
            );
            GatewayResponse::KeyReleaseResult {
                api_key: None,
                error: Some(e.to_string()),
            }
        }
    }
}

/// Maximum params size for Intent messages (64KB)
const INTENT_PARAMS_MAX_SIZE_BYTES: usize = 64 * 1024;

#[allow(clippy::too_many_arguments)]
pub async fn handle_intent_send(
    target: &str,
    action: &str,
    params: &serde_json::Value,
    async_: bool,
    from: &str,
    state: &SharedState,
    grpc_session_mgr: &crate::compat::SharedGrpcSessionMgr,
    bridge_ctrl_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) -> GatewayResponse {
    tracing::info!(
        "IntentSend from={} to={} action={} async={}",
        from,
        target,
        action,
        async_
    );

    // S4.1: Generate message ID for correlation
    let message_id = format!("msg-{}", chrono::Utc::now().timestamp_millis());

    // S4.1.5: Error handling — validate target format
    if target.is_empty() {
        tracing::warn!("IntentSend rejected: empty target");
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:empty-target-{}", message_id),
        };
    }

    // Special handling: target is the HTTP/WebSocket client (not an Agent)
    // When an Agent sends a response back to the Desktop App, it targets
    // "http-api" or "http-ws". We forward via the bridge channel instead
    // of routing through the normal Intent system.
    if target == "http-api" || target == "http-ws" {
        tracing::info!(
            "IntentSend to HTTP client: from={} action={} msg={}",
            from,
            action,
            message_id
        );

        // ADR-021 Phase 2: All events go through the single ctrl channel.
        // Data channel removed — frontend polls via HTTP for message data.
        let event_type = crate::http::routes::BridgeEventType::from_action(action)
            .unwrap_or_else(crate::http::routes::BridgeEventType::default_for_unknown);

        if let Some(tx) = bridge_ctrl_tx.as_ref() {
            // Determine event type based on action
            // Transparent passthrough: Gateway is a dumb pipe, not a protocol
            // translator. Only the Chunk event needs a minimal rename (content→delta)
            // to match the frontend's long-established streaming protocol.
            // All other events pass through Runtime's original params verbatim —
            // the frontend reads fields directly (data.content, data.message, etc.).
            let mut payload = params.clone();
            if event_type == crate::http::routes::BridgeEventType::Chunk {
                // Rename "content" → "delta" for the streaming text protocol.
                // All other fields (reasoning_content, session_id, etc.) are
                // preserved from the original Runtime params.
                if let Some(content) = payload.get("content").and_then(|v| v.as_str()) {
                    payload["delta"] = serde_json::Value::String(content.to_string());
                }
            }

            let event = crate::http::routes::BridgeEvent {
                agent_id: from.to_string(),
                message_id: message_id.clone(),
                event_type,
                payload,
            };

            if let Err(e) = tx.send(event) {
                tracing::warn!("Failed to broadcast bridge event: {}", e);
            }
        } else {
            tracing::warn!("No bridge channel available for HTTP response");
        }

        return GatewayResponse::IntentDelivered {
            message_id: message_id.clone(),
        };
    }

    // S2.4: Params size limit (64KB)
    let params_size = params.to_string().len();
    if params_size > INTENT_PARAMS_MAX_SIZE_BYTES {
        tracing::warn!(
            "IntentSend rejected: params too large ({} bytes, max {} bytes)",
            params_size,
            INTENT_PARAMS_MAX_SIZE_BYTES
        );
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:params-too-large:{}bytes", params_size),
        };
    }

    // S2.4: Capability match check — target must declare the requested action
    let capability_match = {
        let guard = state.read().await;
        guard.capability_registry.has_action(target, action)
    };
    if !capability_match {
        tracing::warn!(
            "IntentSend rejected: target '{}' does not declare action '{}'",
            target,
            action
        );
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:capability-not-found:{}:{}", target, action),
        };
    }

    // S4.1.1: Check if target agent is installed
    let target_installed = {
        let guard = state.read().await;
        guard.is_installed(target)
    };

    if !target_installed {
        tracing::warn!("IntentSend rejected: agent not found: {}", target);
        // S4.1.5: AgentNotFound error — return IntentDelivered with error prefix
        return GatewayResponse::IntentDelivered {
            message_id: format!("error:agent-not-found:{}", target),
        };
    }

    // S4.1.2: Check if target is running
    let target_running = {
        let guard = state.read().await;
        guard.is_running(target)
    };

    if !target_running {
        // S4.1.2: Target not running — need auto-spawn
        // This is coordinated by the Gateway layer (LifecycleManager)
        tracing::info!(
            "IntentSend: target '{}' not running, auto-spawn needed",
            target
        );
    } else {
        // S4.1.3: Target is running — push IntentReceived to target Agent
        let intent_msg = GatewayResponse::IntentReceived {
            from: from.to_string(),
            action: action.to_string(),
            params: params.clone(),
            command: None,
        };
        let pushed = {
            let mgr = grpc_session_mgr.lock().await;
            mgr.push_to_agent(target, intent_msg).await
        };

        if pushed {
            tracing::info!(
                "Intent forwarded: from={} to={} action={}",
                from,
                target,
                action,
            );
        } else {
            tracing::warn!(
                "Intent push failed: target {} channel closed",
                target,
            );
        }
    }

    // S4.1.4: For async intents, the response will be delivered via callback
    if async_ {
        tracing::info!("Async Intent queued: msg={}", message_id);
    }

    GatewayResponse::IntentDelivered { message_id }
}

/// S4.3.3: Budget query handler — returns real remaining budget
pub async fn handle_budget_query(provider: &str, state: &SharedState) -> GatewayResponse {
    let guard = state.read().await;
    if let Some(tracker) = guard.budget_tracker() {
        let remaining = tracker.remaining_tokens(provider);
        let remaining_cost = tracker.remaining_cost_usd(provider);
        tracing::info!(
            "BudgetQuery: provider={} remaining_tokens={} remaining_cost={}",
            provider,
            remaining,
            remaining_cost
        );
        GatewayResponse::BudgetInfo {
            remaining_tokens: remaining,
            remaining_cost_usd: remaining_cost,
        }
    } else {
        // No budget tracker configured — return unlimited
        GatewayResponse::BudgetInfo {
            remaining_tokens: u64::MAX,
            remaining_cost_usd: f64::MAX,
        }
    }
}

/// S4.3.2: Usage report handler — updates cumulative usage
pub async fn handle_usage_report(
    report: acowork_core::budget::UsageReport,
    state: &SharedState,
) -> GatewayResponse {
    tracing::info!(
        "UsageReport: agent={} provider={} tokens={} cost={:.4}",
        report.agent_id,
        report.provider,
        report.tokens_used,
        report.cost_usd
    );

    let mut guard = state.write().await;
    if let Some(tracker) = guard.budget_tracker_mut() {
        tracker.record_usage(
            &report.agent_id,
            &report.provider,
            report.tokens_used,
            report.cost_usd,
        );
    }

    GatewayResponse::UsageReportAck {}
}

/// S4.4.2: Rate acquire handler — token bucket allocation
pub async fn handle_rate_acquire(provider: &str, state: &SharedState) -> GatewayResponse {
    let mut guard = state.write().await;
    if let Some(limiter) = guard.rate_limiter_mut() {
        let result = limiter.try_acquire_for(provider, "default");
        tracing::info!(
            "RateAcquire: provider={} granted={} retry_after={:?}",
            provider,
            result.granted,
            result.retry_after_ms
        );
        GatewayResponse::RateToken {
            granted: result.granted,
            retry_after_ms: result.retry_after_ms,
        }
    } else {
        // No rate limiter configured — always grant
        GatewayResponse::RateToken {
            granted: true,
            retry_after_ms: None,
        }
    }
}

/// Handle CapabilityQuery request from Runtime.
///
/// S4.2.4: Returns the capability registry for the requested agent
/// or all agents if no filter is specified.
pub async fn handle_capability_query(
    agent_id: Option<&str>,
    state: &SharedState,
) -> GatewayResponse {
    let guard = state.read().await;
    let overview = guard.capability_registry.overview();

    match agent_id {
        Some(id) => {
            // Filter to specific agent
            let mut filtered = std::collections::HashMap::new();
            if let Some(actions) = overview.by_agent.get(id) {
                filtered.insert(id.to_string(), actions.clone());
            }
            tracing::info!("CapabilityQuery: agent={:?}, found={}", id, filtered.len());
            GatewayResponse::CapabilityOverview {
                capabilities: filtered,
            }
        }
        None => {
            tracing::info!(
                "CapabilityQuery: all agents, count={}",
                overview.by_agent.len()
            );
            GatewayResponse::CapabilityOverview {
                capabilities: overview.by_agent,
            }
        }
    }
}

// ── Cron handlers (S3.4) ──────────────────────────────────────────────────

pub async fn handle_cron_register(
    agent_id: &str,
    schedule: &str,
    action: &str,
    params: &serde_json::Value,
    state: &SharedState,
) -> GatewayResponse {
    let (cron_id, store_clone) = {
        let mut guard = state.write().await;
        match guard
            .cron_scheduler
            .register(agent_id, schedule, action, params.clone())
        {
            Ok(id) => {
                let store = guard.cron_store.clone();
                (id, store)
            }
            Err(e) => {
                tracing::warn!(
                    "Cron register failed: agent={} schedule={} error={}",
                    agent_id,
                    schedule,
                    e
                );
                return GatewayResponse::CronRegisterResult {
                    cron_id: None,
                    error: Some(e),
                };
            }
        }
    };

    // P1-9 fix: Use spawn_blocking for file I/O in CronStore
    if let Some(store) = store_clone {
        let entry = crate::cron::StoredCronEntry {
            id: cron_id.clone(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params: serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string()),
            timezone: None,
            retry_count: 0,
            retry_interval_secs: 60,
            max_runs: None,
            run_count: 0,
            expires_at: None,
        };
        let cron_id_clone = cron_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.insert(&entry) {
                tracing::warn!("Failed to persist cron entry {}: {}", cron_id_clone, e);
            }
        })
        .await;
    }

    tracing::info!(
        "Cron registered via gRPC: agent={} cron_id={} schedule={} action={}",
        agent_id,
        cron_id,
        schedule,
        action
    );
    GatewayResponse::CronRegisterResult {
        cron_id: Some(cron_id),
        error: None,
    }
}

pub async fn handle_cron_unregister(cron_id: &str, state: &SharedState) -> GatewayResponse {
    let (removed, store_clone) = {
        let mut guard = state.write().await;
        let removed = guard.cron_scheduler.unregister(cron_id);
        let store = guard.cron_store.clone();
        (removed, store)
    };

    // P1-9 fix: Use spawn_blocking for file I/O in CronStore
    if removed && let Some(store) = store_clone {
        let cron_id_clone = cron_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.delete(&cron_id_clone) {
                tracing::warn!(
                    "Failed to delete cron entry {} from store: {}",
                    cron_id_clone,
                    e
                );
            }
        })
        .await;
    }

    tracing::info!("Cron unregister: cron_id={} removed={}", cron_id, removed);
    GatewayResponse::CronUnregisterResult { removed }
}

pub async fn handle_cron_list(
    agent_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    let guard = state.read().await;
    let entries = guard
        .cron_scheduler
        .entries_for_agent(agent_id)
        .into_iter()
        .map(|e| acowork_core::protocol::CronEntryInfo {
            id: e.id.clone(),
            agent_id: e.agent_id.clone(),
            schedule: e.schedule.clone(),
            action: e.action.clone(),
            params: e.params.clone(),
            timezone: e.timezone.clone(),
            retry_count: e.retry_count,
            retry_interval_secs: e.retry_interval_secs,
            max_runs: e.max_runs,
            run_count: e.run_count,
            expires_at: e.expires_at,
        })
        .collect();

    GatewayResponse::CronListResult { entries }
}

/// Handle ContextUsageReport — forward context usage to Desktop App via WebSocket bridge
pub async fn handle_context_usage_report(
    agent_id: &str,
    session_id: &str,
    context: &acowork_core::protocol::ContextUsageInfo,
    bridge_ctrl_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) -> GatewayResponse {
    tracing::info!(
        agent = %agent_id,
        session = %session_id,
        context_window = context.context_window,
        total_tokens = context.total_tokens,
        has_bridge = bridge_ctrl_tx.is_some(),
        "ContextUsageReport received from Runtime"
    );
    // Broadcast context_usage event to all WebSocket bridge subscribers
    if let Some(tx) = bridge_ctrl_tx {
        // Inject session_id into the payload so the frontend can route
        // the event to the correct session (not just the active one).
        let mut payload = serde_json::to_value(context).unwrap_or_default();
        if let serde_json::Value::Object(ref mut map) = payload {
            map.insert("session_id".to_string(), serde_json::Value::String(session_id.to_string()));
        }
        let event = crate::http::routes::BridgeEvent {
            agent_id: agent_id.to_string(),
            message_id: String::new(),
            event_type: crate::http::routes::BridgeEventType::ContextUsage,
            payload,
        };
        match tx.send(event) {
            Ok(count) => {
                tracing::info!(agent = %agent_id, receivers = count, "ContextUsage broadcast to WS bridge")
            }
            Err(e) => tracing::warn!("Failed to forward context_usage to bridge: {}", e),
        }
    } else {
        tracing::warn!(
            agent = %agent_id,
            "ContextUsage: NO bridge_ctrl_tx — WS bridge not connected, event dropped"
        );
    }
    GatewayResponse::ContextUsageAck {}
}

/// Handle AgentHello — register the session with the agent's identity
///
/// On successful authentication, bundles all handshake-time configuration
/// (LLM config, workspace context, runtime overrides) into the AgentHelloResult
/// response.  Resource lists use version-driven diff sync:
/// - provider_list / mcp_list / search_list are only sent when Runtime's cached version < Gateway's.
/// - provider_key_vault / mcp_key_vault / search_key_vault are always sent in full (keys not versioned).
///   This satisfies PRD GTW-05 and SEC-07: API keys are distributed via gRPC,
///   not environment variables.
#[allow(clippy::too_many_arguments)]
pub async fn handle_agent_hello(
    agent_id: &str,
    version: &str,
    connection_role: &str,
    provider_list_version: u64,
    mcp_list_version: u64,
    search_list_version: u64,
    user_profile_version: u64,
    avatar: Option<String>,
    builtin_avatar: Option<String>,
    state: &SharedState,
) -> GatewayResponse {
    tracing::info!(
        "AgentHello received: agent_id={} version={} role={} prov_ver={} mcp_ver={} user_ver={} avatar={:?} builtin={:?}",
        agent_id,
        version,
        connection_role,
        provider_list_version,
        mcp_list_version,
        user_profile_version,
        avatar,
        builtin_avatar
    );

    // Mark the agent as connected in GatewayState
    {
        let mut gw = state.write().await;
        gw.set_agent_connected(agent_id, true);

        // ADR-017: Sync avatar from Runtime's agent_config.json to the
        // Gateway's avatar cache + in-memory manifest. This handles
        // recovery of avatar changes made while the old Gateway was running.
        if avatar.is_some() || builtin_avatar.is_some() {
            if let Some(info) = gw.installed_agents.get_mut(agent_id) {
                info.manifest.avatar = avatar.clone();
                info.manifest.builtin_avatar = builtin_avatar.clone();
            }
            // Also persist to the avatar cache file.
            if let Some(ref config) = gw.config {
                let data_dir = std::path::PathBuf::from(&config.data_dir);
                crate::http::agent_config::update_avatar_in_cache(
                    &data_dir,
                    agent_id,
                    avatar.clone(),
                    builtin_avatar.clone(),
                );
            }
        }
    }

    // ── Build resource lists from in-memory cache ─────────────────
    let gw = state.read().await;

    let (provider_list, gw_provider_version) =
        if provider_list_version < gw.resource_cache.provider_list.version {
            (
                Some(gw.resource_cache.provider_list.providers.clone()),
                gw.resource_cache.provider_list.version,
            )
        } else {
            (None, gw.resource_cache.provider_list.version)
        };

    let (mcp_list, gw_mcp_version) = if mcp_list_version < gw.resource_cache.mcp_list.version {
        (
            Some(gw.resource_cache.mcp_list.servers.clone()),
            gw.resource_cache.mcp_list.version,
        )
    } else {
        (None, gw.resource_cache.mcp_list.version)
    };

    let (search_list, gw_search_version) =
        if search_list_version < gw.resource_cache.search_list.version {
            (
                Some(gw.resource_cache.search_list.providers.clone()),
                gw.resource_cache.search_list.version,
            )
        } else {
            (None, gw.resource_cache.search_list.version)
        };

    // ── Key vaults (always full, from Vault + MCP catalog) ─────
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

    // Load MCP catalog for key extraction
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));
    let mcp_key_vault = match crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir) {
        Ok(catalog) => crate::resource_cache::build_mcp_key_vault(&catalog),
        Err(_) => Vec::new(),
    };

    // Build search key vault (always full delivery)
    let search_key_vault = crate::resource_cache::build_search_key_vault(&gw);

    // ── Embedding service info (from embed_process state) ──
    let embed_endpoint = gw
        .embed_process
        .as_ref()
        .map(|eps| format!("http://127.0.0.1:{}/v1", eps.port));
    let embed_model_id = gw
        .embed_process
        .as_ref()
        .and_then(|eps| eps.active_model_id.clone());
    let embed_dimension = gw
        .embed_process
        .as_ref()
        .and_then(|eps| eps.active_dimension);

    // ── LSP Relay endpoint (from lsp_relay_process state) ──
    let lsp_relay_endpoint = gw
        .lsp_relay_process
        .as_ref()
        .filter(|eps| eps.ready)
        .map(|eps| format!("http://127.0.0.1:{}", eps.port));

    drop(gw);

    // ── User identity (version-driven diff sync) ──
    let (user_identity, gw_user_version) = {
        let gw = state.read().await;
        let active_user = gw
            .resource_cache
            .user_profile_list
            .users
            .iter()
            .find(|u| u.is_active)
            .cloned();
        (active_user, gw.resource_cache.user_profile_list.version)
    };

    GatewayResponse::AgentHelloResult {
        success: true,
        error: None,
        provider_list,
        provider_list_version: gw_provider_version,
        mcp_list,
        mcp_list_version: gw_mcp_version,
        provider_key_vault,
        mcp_key_vault,
        search_list,
        search_list_version: gw_search_version,
        search_key_vault,
        user_identity,
        user_profile_version: gw_user_version,
        embed_endpoint,
        embed_model_id,
        embed_dimension,
        lsp_relay_endpoint,
    }
}

/// Handle AgentReady — marks the agent as ready to receive messages.
///
/// Called by Runtime after SessionTask initialization is complete.
/// This enables the Desktop App to know when it's safe to open WebSocket
/// connections for chat streaming.
pub async fn handle_agent_ready(agent_id: &str, state: &SharedState) -> GatewayResponse {
    tracing::info!("AgentReady: agent_id={}", agent_id);

    let mut gw = state.write().await;
    gw.set_agent_ready(agent_id, true);

    GatewayResponse::UsageReportAck {} // Simple acknowledgment
}

/// Resolved LLM configuration for an Agent.
///
/// Returned by `resolve_llm_config_for_agent`, replaces the previous
/// 6-tuple with named fields for readability and maintainability.
pub struct ResolvedLlmConfig {
    pub provider: String,
    pub model: Option<String>,
    pub api_key: String,
    pub base_url: Option<String>,
    pub models: Vec<String>,
    /// Capabilities for ALL models in this provider, so Runtime can
    /// look them up when switching models without a Gateway roundtrip.
    pub all_model_capabilities: Vec<acowork_core::protocol::ProviderModelEntry>,
    pub compact_model: Option<String>,
}

/// Resolve the LLM configuration to deliver to an Agent.
///
/// Config (base_url, models, capabilities, compact_model) comes from
/// provider_list.json (resource_cache). API key comes from Vault.
///
/// Priority:
/// 1. Gateway config `default_provider` + `default_model`
/// 2. First provider in provider_list.json
/// 3. None (Agent has no provider configured)
///
/// Model resolution order (within the chosen provider):
/// 1. Gateway config `default_model` (explicit user choice)
/// 2. Provider config's models[0]
/// 3. None — Agent Runtime uses the first model from the provider list
pub async fn resolve_llm_config_for_agent(
    _agent_id: &str,
    state: &SharedState,
) -> Option<ResolvedLlmConfig> {
    let state_guard = state.read().await;

    // Try default_provider from Gateway config first
    let default_provider = state_guard
        .config
        .as_ref()
        .and_then(|c| c.default_provider.as_deref());

    // Try default_model from Gateway config
    let config_default_model = state_guard
        .config
        .as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Determine which provider to use
    let provider_name = if let Some(name) = default_provider {
        Some(name.to_string())
    } else {
        // Fall back to first provider in resource cache
        state_guard
            .resource_cache
            .provider_list
            .providers
            .first()
            .map(|p| p.id.clone())
    };

    let provider_name = match provider_name {
        Some(name) => name,
        None => {
            tracing::info!("No provider configured, cannot deliver LLM config");
            return None;
        }
    };

    // Look up provider config from resource_cache (provider_list.json).
    let provider_config = state_guard
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider_name)
        .cloned();

    let provider_config = match provider_config {
        Some(cfg) => cfg,
        None => {
            tracing::warn!(
                "Provider '{}' not found in resource cache",
                provider_name
            );
            return None;
        }
    };

    let models: Vec<String> = provider_config
        .models
        .iter()
        .map(|m| m.id.clone())
        .collect();

    // Model resolution: gateway config default_model > provider's models[0]
    let model = config_default_model
        .map(|m| m.to_string())
        .or_else(|| provider_config.models.first().map(|m| m.id.clone()));

    // Get API key from Vault.
    let api_key = state_guard
        .vault
        .get_key(&provider_name)
        .ok()
        .unwrap_or_default();

    Some(ResolvedLlmConfig {
        provider: provider_name,
        model,
        api_key,
        base_url: if provider_config.base_url.is_empty() {
            None
        } else {
            Some(provider_config.base_url)
        },
        models,
        all_model_capabilities: provider_config.models,
        compact_model: provider_config.compact_model,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-ipc-state-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn test_shared_state(name: &str) -> SharedState {
        let dir = temp_vault_dir(name);
        Arc::new(RwLock::new(GatewayState::new(&dir)))
    }

    // ── Unit tests for handlers (async, with state) ──────────────────────

    #[tokio::test]
    async fn test_handle_budget_query() {
        let state = test_shared_state("budget-query");
        let response = handle_budget_query("openai", &state).await;
        if let GatewayResponse::BudgetInfo {
            remaining_tokens, ..
        } = response
        {
            // No budget tracker configured → unlimited
            assert_eq!(remaining_tokens, u64::MAX);
        } else {
            panic!("Expected BudgetInfo");
        }
    }

    #[tokio::test]
    async fn test_handle_rate_acquire() {
        let state = test_shared_state("rate-acquire");
        let response = handle_rate_acquire("openai", &state).await;
        if let GatewayResponse::RateToken {
            granted,
            retry_after_ms,
        } = response
        {
            // No rate limiter configured → always grant
            assert!(granted);
            assert!(retry_after_ms.is_none());
        } else {
            panic!("Expected RateToken");
        }
    }

    // ── Permission-related tests removed (old dual-authorization layer deleted) ──
    // See: be7bd1c "权限体系重构 — 删除双授权层 + Shell 命令风险审批机制"

    #[tokio::test]
    async fn test_handle_usage_report() {
        let state = test_shared_state("usage-report");
        let report = acowork_core::budget::UsageReport {
            agent_id: "com.example.weather".to_string(),
            provider: "openai".to_string(),
            tokens_used: 150,
            cost_usd: 0.01,
            timestamp: chrono::Utc::now(),
            error: None,
        };
        let response = handle_usage_report(report, &state).await;
        assert!(matches!(response, GatewayResponse::UsageReportAck {}));
    }

    // ── Integration tests (no longer using legacy IPC transport) ─────

    #[tokio::test]
    async fn test_gateway_state_concurrent_access() {
        let dir = temp_vault_dir("concurrent_rw");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

        let mut handles = Vec::new();

        // Concurrent reads (should not block each other with RwLock)
        for _ in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let guard = state.read().await;
                assert!(guard.installed_agents.is_empty());
            }));
        }

        // Concurrent writes
        for i in 0..5 {
            let state = Arc::clone(&state);
            handles.push(tokio::spawn(async move {
                let mut guard = state.write().await;
                let toml_str = r#"
                    agent_id = "com.test"
                    version = "1.0.0"
                    name = "Test"
                    description = "test"
                    author = "test"
                    runtime_version = "0.1.0"
                    [llm]
                    provider = "openai"
                    model = "gpt-4"
                "#;
                let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
                guard.add_installed(crate::gateway::state::AgentInfo {
                    agent_id: format!("com.test.{}", i),
                    version: "1.0.0".to_string(),
                    name: format!("Test Agent {}", i),
                    install_path: "/tmp/test".to_string(),
                    manifest,
                });
            }));
        }

        // All tasks should complete without deadlock
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all writes succeeded
        {
            let guard = state.read().await;
            assert_eq!(guard.installed_agents.len(), 5);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }


    /// S2.4: Test IntentSend rejected when target lacks the requested capability
    #[tokio::test]
    async fn test_intent_send_capability_mismatch() {
        let dir = temp_vault_dir("intent_no_cap");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        let grpc_session_mgr: crate::compat::SharedGrpcSessionMgr = Arc::new(
            tokio::sync::Mutex::new(crate::compat::GrpcSessionManager::new()),
        );

        // Install target (but don't register any capability)
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
        }

        let response = handle_intent_send(
            "com.example.target",
            "nonexistent_action",
            &serde_json::json!({}),
            false,
            "com.example.sender",
            &state,
            &grpc_session_mgr,
            &None,
        )
        .await;

        if let GatewayResponse::IntentDelivered { message_id } = &response {
            assert!(
                message_id.starts_with("error:capability-not-found"),
                "Expected capability-not-found error, got: {}",
                message_id
            );
        } else {
            panic!("Expected IntentDelivered with error, got {:?}", response);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2.4: Test IntentSend rejected when params exceed 64KB limit
    #[tokio::test]
    async fn test_intent_send_params_too_large() {
        let dir = temp_vault_dir("intent_large_params");
        let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));
        let grpc_session_mgr: crate::compat::SharedGrpcSessionMgr = Arc::new(
            tokio::sync::Mutex::new(crate::compat::GrpcSessionManager::new()),
        );

        // Install target with capability
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "target agent"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
            guard.capability_registry.register(
                "com.example.target",
                "weather_query",
                acowork_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Create params > 64KB
        let large_data = "x".repeat(65 * 1024);
        let large_params = serde_json::json!({"data": large_data});

        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &large_params,
            false,
            "com.example.sender",
            &state,
            &grpc_session_mgr,
            &None,
        )
        .await;

        if let GatewayResponse::IntentDelivered { message_id } = &response {
            assert!(
                message_id.starts_with("error:params-too-large"),
                "Expected params-too-large error, got: {}",
                message_id
            );
        } else {
            panic!("Expected IntentDelivered with error, got {:?}", response);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn test_capability_broadcast_to_sessions() {
        let (capability_tx, mut cap_rx1) = tokio::sync::broadcast::channel::<GatewayResponse>(64);
        let mut cap_rx2 = capability_tx.subscribe();

        // Simulate an install event — broadcast CapabilityUpdate
        let update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec!["query".to_string(), "forecast".to_string()],
            removed: false,
        };
        capability_tx.send(update.clone()).unwrap();

        // Both subscribers should receive the update
        let msg1 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx1.recv())
            .await
            .expect("Timeout waiting for broadcast on subscriber 1")
            .expect("Channel closed");

        let msg2 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx2.recv())
            .await
            .expect("Timeout waiting for broadcast on subscriber 2")
            .expect("Channel closed");

        match (&msg1, &msg2) {
            (
                GatewayResponse::CapabilityUpdate {
                    agent_id,
                    actions,
                    removed,
                },
                GatewayResponse::CapabilityUpdate { .. },
            ) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(actions.len(), 2);
                assert!(!removed);
            }
            _ => panic!("Expected CapabilityUpdate, got {:?} and {:?}", msg1, msg2),
        }

        // Simulate an uninstall event
        let remove_update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec![],
            removed: true,
        };
        capability_tx.send(remove_update.clone()).unwrap();

        let msg3 = tokio::time::timeout(std::time::Duration::from_millis(500), cap_rx1.recv())
            .await
            .expect("Timeout waiting for uninstall broadcast")
            .expect("Channel closed");

        match &msg3 {
            GatewayResponse::CapabilityUpdate {
                agent_id,
                actions,
                removed,
            } => {
                assert_eq!(agent_id, "com.example.weather");
                assert!(actions.is_empty());
                assert!(*removed);
            }
            _ => panic!("Expected CapabilityUpdate (removed), got {:?}", msg3),
        }
    }
}
