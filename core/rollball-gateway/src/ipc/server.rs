//! Gateway Service API handler implementations
//!
//! Contains handler functions for processing Gateway Service API requests.
//! These handlers are shared between the gRPC server (grpc/dispatch.rs)
//! and can be used by any transport layer.

use std::sync::Arc;
use tokio::sync::{RwLock, Mutex};

use rollball_core::protocol::GatewayResponse;
use crate::gateway::state::GatewayState;
use crate::ipc::session::SessionManager;

/// Shared state type: Arc<RwLock<GatewayState>> for concurrent read/write access.
/// RwLock chosen because handlers are predominantly read-heavy (key lookup,
/// budget query) with occasional writes (install/uninstall).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Shared session manager type
pub type SharedSessionMgr = Arc<Mutex<SessionManager>>;

// ── Handler implementations ─────────────────────────────────────────────────

pub async fn handle_key_release(
    provider: &str,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    // Check if session is authenticated (read-only on session_mgr)
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };
    // Session lock released before acquiring state lock — avoids deadlocks

    match agent_id {
        Some(id) => {
            // Read-only access to GatewayState
            let state_guard = state.read().await;
            match state_guard.vault.get_key(provider) {
                Ok(api_key) => {
                    tracing::info!(
                        "KeyRelease for agent={}, provider={}",
                        id,
                        provider
                    );
                    GatewayResponse::KeyReleaseResult {
                        api_key: Some(api_key),
                        error: None,
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "KeyRelease failed for agent={}, provider={}: {}",
                        id,
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
        None => {
            tracing::warn!(
                "KeyRelease from unauthenticated session {}",
                conn_id
            );
            GatewayResponse::KeyReleaseResult {
                api_key: None,
                error: Some("unauthenticated session".into()),
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
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) -> GatewayResponse {
    let from = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id)
            .and_then(|s| s.agent_id.clone())
            .unwrap_or_else(|| "unknown".to_string())
    };

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
            from, action, message_id
        );

        if let Some(tx) = bridge_tx {
            // Determine event type based on action
            let event_type = crate::http::routes::BridgeEventType::from_action(action)
                .unwrap_or_else(crate::http::routes::BridgeEventType::default_for_unknown);

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
                agent_id: from.clone(),
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
            params_size, INTENT_PARAMS_MAX_SIZE_BYTES
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
            target, action
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
        tracing::info!("IntentSend: target '{}' not running, auto-spawn needed", target);
    } else {
        // S4.1.3: Target is running — push IntentReceived to target Agent
        let target_conn_id = {
            let mgr = session_mgr.lock().await;
            mgr.find_by_agent_id(target).map(|(conn_id, _)| conn_id.clone())
        };

        if let Some(target_conn) = target_conn_id {
            let pushed = {
                let mgr = session_mgr.lock().await;
                if let Some(session) = mgr.get_session(&target_conn) {
                    let intent_msg = GatewayResponse::IntentReceived {
                        from: from.clone(),
                        action: action.to_string(),
                        params: params.clone(),
                        command: None,
                    };
                    session.push_message(intent_msg).await
                } else {
                    false
                }
            };

            if pushed {
                tracing::info!(
                    "Intent forwarded: from={} to={} action={} via conn={}",
                    from, target, action, target_conn
                );
            } else {
                tracing::warn!(
                    "Intent push failed: target {} conn {} channel closed",
                    target, target_conn
                );
            }
        } else {
            tracing::warn!(
                "Intent target '{}' is running but has no IPC session",
                target
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
            provider, remaining, remaining_cost
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
    report: rollball_core::budget::UsageReport,
    state: &SharedState,
) -> GatewayResponse {
    tracing::info!(
        "UsageReport: agent={} provider={} tokens={} cost={:.4}",
        report.agent_id, report.provider, report.tokens_used, report.cost_usd
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
            provider, result.granted, result.retry_after_ms
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
            tracing::info!("CapabilityQuery: all agents, count={}", overview.by_agent.len());
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
        match guard.cron_scheduler.register(agent_id, schedule, action, params.clone()) {
            Ok(id) => {
                let store = guard.cron_store.clone();
                (id, store)
            }
            Err(e) => {
                tracing::warn!("Cron register failed: agent={} schedule={} error={}", agent_id, schedule, e);
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
        }).await;
    }

    tracing::info!(
        "Cron registered via IPC: agent={} cron_id={} schedule={} action={}",
        agent_id, cron_id, schedule, action
    );
    GatewayResponse::CronRegisterResult {
        cron_id: Some(cron_id),
        error: None,
    }
}

pub async fn handle_cron_unregister(
    cron_id: &str,
    state: &SharedState,
) -> GatewayResponse {
    let (removed, store_clone) = {
        let mut guard = state.write().await;
        let removed = guard.cron_scheduler.unregister(cron_id);
        let store = guard.cron_store.clone();
        (removed, store)
    };

    // P1-9 fix: Use spawn_blocking for file I/O in CronStore
    if removed
        && let Some(store) = store_clone
    {
        let cron_id_clone = cron_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.delete(&cron_id_clone) {
                tracing::warn!("Failed to delete cron entry {} from store: {}", cron_id_clone, e);
            }
        }).await;
    }

    tracing::info!("Cron unregister: cron_id={} removed={}", cron_id, removed);
    GatewayResponse::CronUnregisterResult { removed }
}

pub async fn handle_cron_list(
    conn_id: &str,
    session_mgr: &SharedSessionMgr,
    state: &SharedState,
) -> GatewayResponse {
    // Get agent_id from session
    let agent_id = {
        let mgr = session_mgr.lock().await;
        mgr.get_session(conn_id).and_then(|s| s.agent_id.clone())
    };

    let agent_id = match agent_id {
        Some(id) => id,
        None => {
            return GatewayResponse::CronListResult { entries: vec![] };
        }
    };

    let guard = state.read().await;
    let entries = guard.cron_scheduler
        .entries_for_agent(&agent_id)
        .into_iter()
        .map(|e| rollball_core::protocol::CronEntryInfo {
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
    context: &rollball_core::protocol::ContextUsageInfo,
    _conn_id: &str,
    _session_mgr: &SharedSessionMgr,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<crate::http::routes::BridgeEvent>>,
) -> GatewayResponse {
    tracing::info!(
        agent = %agent_id,
        context_window = context.context_window,
        total_tokens = context.total_tokens,
        has_bridge = bridge_tx.is_some(),
        "ContextUsageReport received from Runtime"
    );
    // Broadcast context_usage event to all WebSocket bridge subscribers
    if let Some(tx) = bridge_tx {
        let event = crate::http::routes::BridgeEvent {
            agent_id: agent_id.to_string(),
            message_id: String::new(),
            event_type: crate::http::routes::BridgeEventType::ContextUsage,
            payload: serde_json::to_value(context).unwrap_or_default(),
        };
        match tx.send(event) {
            Ok(count) => tracing::info!(agent = %agent_id, receivers = count, "ContextUsage broadcast to WS bridge"),
            Err(e) => tracing::warn!("Failed to forward context_usage to bridge: {}", e),
        }
    } else {
        tracing::warn!(
            agent = %agent_id,
            "ContextUsage: NO bridge_tx — WS bridge not connected, event dropped"
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
/// This satisfies PRD GTW-05 and SEC-07: API keys are distributed via IPC,
/// not environment variables.
pub async fn handle_agent_hello(
    agent_id: &str,
    version: &str,
    connection_role: &str,
    provider_list_version: u64,
    mcp_list_version: u64,
    search_list_version: u64,
    user_profile_version: u64,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &SharedSessionMgr,
) -> GatewayResponse {
    tracing::info!(
        "AgentHello received: agent_id={} version={} conn={} role={} prov_ver={} mcp_ver={} user_ver={}",
        agent_id, version, conn_id, connection_role, provider_list_version, mcp_list_version, user_profile_version
    );

    let mut mgr = session_mgr.lock().await;
    if let Some(session) = mgr.get_session_mut(conn_id) {
        session.authenticate(agent_id);
        session.connection_role = connection_role.to_string();
        tracing::info!("Session {} authenticated as agent {} (role={})", conn_id, agent_id, connection_role);

        // Mark the agent as connected in GatewayState
        {
            let mut gw = state.write().await;
            gw.set_agent_connected(agent_id, true);
        }

        // ── Build resource lists from in-memory cache ─────────────────
        let gw = state.read().await;

        let (provider_list, gw_provider_version) = if provider_list_version < gw.resource_cache.provider_list.version {
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

        let (search_list, gw_search_version) = if search_list_version < gw.resource_cache.search_list.version {
            (
                Some(gw.resource_cache.search_list.providers.clone()),
                gw.resource_cache.search_list.version,
            )
        } else {
            (None, gw.resource_cache.search_list.version)
        };

        // ── Key vaults (always full, from Vault + MCP catalog) ─────
        let provider_key_vault: Vec<rollball_core::protocol::ProviderKeyEntry> = gw
            .vault
            .list_providers()
            .iter()
            .filter_map(|name| {
                gw.vault.get_provider(name).ok().map(|entry| {
                    rollball_core::protocol::ProviderKeyEntry {
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

        drop(gw);

        // ── User identity (version-driven diff sync) ──
        let (user_identity, gw_user_version) = {
            let gw = state.read().await;
            let active_user = gw.resource_cache.user_profile_list.users
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
        }
    } else {
        tracing::warn!("AgentHello from unknown connection {}", conn_id);
        GatewayResponse::AgentHelloResult {
            success: false,
            error: Some(format!("Unknown connection: {}", conn_id)),
            provider_list: None,
            provider_list_version: 0,
            mcp_list: None,
            mcp_list_version: 0,
            provider_key_vault: vec![],
            mcp_key_vault: vec![],
            search_list: None,
            search_list_version: 0,
            search_key_vault: vec![],
            user_identity: None,
            user_profile_version: 0,
        }
    }
}

/// Handle AgentReady — marks the agent as ready to receive messages.
///
/// Called by Runtime after SessionTask initialization is complete.
/// This enables the Desktop App to know when it's safe to open WebSocket
/// connections for chat streaming.
pub async fn handle_agent_ready(
    agent_id: &str,
    state: &SharedState,
) -> GatewayResponse {
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
    pub stored_capabilities: Option<rollball_core::protocol::ModelCapabilitiesInfo>,
}

/// Resolve the LLM configuration to deliver to an Agent.
///
/// Priority:
/// 1. Gateway config `default_provider` + `default_model` → look up in Vault
/// 2. First key stored in Vault (with its default_model)
/// 3. None (Agent falls back to manifest suggested_provider + env vars)
///
/// Model resolution order (within the chosen provider):
/// 1. Gateway config `default_model` (explicit user choice)
/// 2. Vault entry's `default_model` (set when adding the provider key)
/// 3. None — Agent Runtime falls back to its manifest's suggested_model
pub async fn resolve_llm_config_for_agent(
    agent_id: &str,
    state: &SharedState,
) -> Option<ResolvedLlmConfig> {
    let state_guard = state.read().await;

    // Try default_provider from Gateway config first
    let default_provider = state_guard.config.as_ref()
        .and_then(|c| c.default_provider.as_deref());

    // Try default_model from Gateway config
    let config_default_model = state_guard.config.as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Determine which provider to use
    let provider_name = if let Some(name) = default_provider {
        Some(name.to_string())
    } else {
        // Fall back to first key in Vault
        state_guard.vault.list_providers().first().cloned()
    };

    let provider_name = match provider_name {
        Some(name) => name,
        None => {
            tracing::info!("No provider configured in Vault, cannot deliver LLM config");
            return None;
        }
    };

    // Retrieve the provider entry from Vault
    match state_guard.vault.get_provider(&provider_name) {
        Ok(entry) => {
            // Model resolution: per-agent preference > config default > Vault default > None
            // 1. Per-agent model preference is owned by the Agent Runtime
            //    (workspace/config/agent_model.json). The Gateway will query it via
            //    QueryConfig IPC when the Runtime is connected.
            let (per_agent_model, per_agent_provider): (Option<String>, Option<String>) = (None, None);

            // 2. Cross-provider resolution: if the per-agent preference
            //    specifies a DIFFERENT provider than the default, look up
            //    THAT provider's vault entry so we can validate the model
            //    against the correct model list and deliver the correct
            //    api_key/base_url to the Agent Runtime.
            //
            //    Pre-clone default entry data so the if-else branches
            //    don't fight over `entry` borrows.
            let default_entry = entry.clone();
            let default_provider_name = provider_name.clone();
            let (effective_entry, effective_models, effective_provider_name) =
                if let Some(ref ap) = per_agent_provider {
                    if ap != &default_provider_name {
                        // Per-agent model is from a different provider
                        match state_guard.vault.get_provider(ap) {
                            Ok(alt_entry) => {
                                tracing::info!(
                                    agent = %agent_id,
                                    default_provider = %default_provider_name,
                                    per_agent_provider = %ap,
                                    "Using per-agent provider for model resolution"
                                );
                                let models = alt_entry.models.clone();
                                (alt_entry, models, ap.clone())
                            }
                            Err(_) => {
                                // Per-agent provider no longer in vault, fall back to default
                                tracing::warn!(
                                    agent = %agent_id,
                                    per_agent_provider = %ap,
                                    "Per-agent provider not found in Vault, falling back to default"
                                );
                                (default_entry.clone(), default_entry.models.clone(), default_provider_name)
                            }
                        }
                    } else {
                        (default_entry.clone(), default_entry.models.clone(), default_provider_name)
                    }
                } else {
                    (default_entry.clone(), default_entry.models.clone(), default_provider_name)
                };

            // 3. Validate per-agent model against effective provider's models
            let per_agent_model = per_agent_model.filter(|m| effective_models.contains(m));

            let model = per_agent_model
                .or(config_default_model.map(|m| m.to_string()))
                .or(effective_entry.default_model.clone());
            // model is None when neither config nor Vault has a preference —
            // Agent Runtime will fall back to its manifest's suggested_model

            Some(ResolvedLlmConfig {
                provider: effective_provider_name,
                model: model.clone(),
                api_key: effective_entry.api_key,
                base_url: effective_entry.base_url,
                models: effective_models,
                stored_capabilities: model.as_ref()
                    .and_then(|m| effective_entry.model_capabilities.get(m))
                    .map(|c| rollball_core::protocol::ModelCapabilitiesInfo::from(c.clone())),
            })
        }
        Err(e) => {
            tracing::warn!("Failed to get provider '{}' from Vault: {}", provider_name, e);
            None
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "rollball-test-ipc-state-{}-{}",
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
        if let GatewayResponse::BudgetInfo { remaining_tokens, .. } = response {
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
        let report = rollball_core::budget::UsageReport {
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
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));

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
                let manifest =
                    rollball_core::AgentManifest::from_toml(toml_str).unwrap();
                guard.add_installed(
                    crate::gateway::state::AgentInfo {
                        agent_id: format!("com.test.{}", i),
                        version: "1.0.0".to_string(),
                        name: format!("Test Agent {}", i),
                        install_path: "/tmp/test".to_string(),
                        manifest,
                    },
                );
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

    /// S4.1.3: Test that IntentSend pushes IntentReceived to the target's session
    #[tokio::test]
    async fn test_intent_push_to_target_session() {
        let dir = temp_vault_dir("intent_push");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

        // Register target's capability
        {
            let mut guard = state.write().await;
            guard.capability_registry.register(
                "com.example.target",
                "weather_query",
                rollball_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Simulate target agent's session with a push channel
        let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<GatewayResponse>(8);
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session_with_push("conn-target", push_tx);
            mgr.get_session_mut("conn-target")
                .unwrap()
                .authenticate("com.example.target");
        }

        // Mark target as installed and running
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
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
            guard.add_running(crate::gateway::state::RunningAgentInfo {
                agent_id: "com.example.target".to_string(),
                pid: 1234,
                started_at: chrono::Utc::now(),
                workspace: "/tmp/test".to_string(),
                connected: false,
                ready: false,
                dev_mode: false,
                debug_port: None,
                workspace_config_json: None,
            });
        }

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        // Call handle_intent_send (no perm_store parameter after refactor)
        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &serde_json::json!({"city": "Shanghai"}),
            false,
            "conn-sender",
            &state,
            &session_mgr,
            &None,
        )
        .await;

        // Verify the immediate response is IntentDelivered
        match &response {
            GatewayResponse::IntentDelivered { message_id } => {
                assert!(!message_id.starts_with("error:"));
            }
            _ => panic!("Expected IntentDelivered, got {:?}", response),
        }

        // Verify the target received IntentReceived via push channel
        let pushed_msg = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            push_rx.recv(),
        )
        .await
        .expect("Timeout waiting for push message")
        .expect("Push channel closed");

        match &pushed_msg {
            GatewayResponse::IntentReceived {
                from,
                action,
                params,
                command: _,
            } => {
                assert_eq!(from, "com.example.sender");
                assert_eq!(action, "weather_query");
                assert_eq!(params["city"], "Shanghai");
            }
            _ => panic!("Expected IntentReceived, got {:?}", pushed_msg),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S2.4: Test IntentSend rejected when target lacks the requested capability
    #[tokio::test]
    async fn test_intent_send_capability_mismatch() {
        let dir = temp_vault_dir("intent_no_cap");
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

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
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
        }

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        let response = handle_intent_send(
            "com.example.target",
            "nonexistent_action",
            &serde_json::json!({}),
            false,
            "conn-sender",
            &state,
            &session_mgr,
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
        let state: SharedState =
            Arc::new(RwLock::new(GatewayState::new(&dir)));
        let session_mgr: SharedSessionMgr = Arc::new(Mutex::new(SessionManager::new()));

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
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
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
                rollball_core::CapabilityDef {
                    description: "Query weather".to_string(),
                    input_schema: None,
                    output_schema: None,
                },
            );
        }

        // Simulate sender's session
        {
            let mut mgr = session_mgr.lock().await;
            mgr.create_session("conn-sender");
            mgr.get_session_mut("conn-sender")
                .unwrap()
                .authenticate("com.example.sender");
        }

        // Create params > 64KB
        let large_data = "x".repeat(65 * 1024);
        let large_params = serde_json::json!({"data": large_data});

        let response = handle_intent_send(
            "com.example.target",
            "weather_query",
            &large_params,
            false,
            "conn-sender",
            &state,
            &session_mgr,
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
        let (capability_tx, mut cap_rx1) =
            tokio::sync::broadcast::channel::<GatewayResponse>(64);
        let mut cap_rx2 = capability_tx.subscribe();

        // Simulate an install event — broadcast CapabilityUpdate
        let update = GatewayResponse::CapabilityUpdate {
            agent_id: "com.example.weather".to_string(),
            actions: vec!["query".to_string(), "forecast".to_string()],
            removed: false,
        };
        capability_tx.send(update.clone()).unwrap();

        // Both subscribers should receive the update
        let msg1 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx1.recv(),
        )
        .await
        .expect("Timeout waiting for broadcast on subscriber 1")
        .expect("Channel closed");

        let msg2 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx2.recv(),
        )
        .await
        .expect("Timeout waiting for broadcast on subscriber 2")
        .expect("Channel closed");

        match (&msg1, &msg2) {
            (
                GatewayResponse::CapabilityUpdate { agent_id, actions, removed },
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

        let msg3 = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            cap_rx1.recv(),
        )
        .await
        .expect("Timeout waiting for uninstall broadcast")
        .expect("Channel closed");

        match &msg3 {
            GatewayResponse::CapabilityUpdate { agent_id, actions, removed } => {
                assert_eq!(agent_id, "com.example.weather");
                assert!(actions.is_empty());
                assert!(*removed);
            }
            _ => panic!("Expected CapabilityUpdate (removed), got {:?}", msg3),
        }
    }
}
