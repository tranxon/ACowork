//! Phase D: announce ready and enter the main Gateway loop.
//!
//! ADR-033: MQTT-only mode (gRPC removed per ADR-034 §8 Phase 2).
//! ADR-034 §8 Phase 2-1: single dispatch table for InboundMessage.
//! ADR-034 §8 Phase 2-2: gRPC path removed.

use crate::cli::LogReloadHandle;
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext};
use crate::startup::subsystems::SubsystemHandles;

use std::sync::Arc;

/// Phase D: notify Gateway that the agent is ready, then run the message loop.
///
/// This is the last phase of the startup sequence.  It runs until the
/// MQTT connection is closed or a fatal error occurs.
///
/// ADR-034 §8 Phase 2-2: gRPC path removed; `log_reload_handle` is no
/// longer used (was only consumed by `run_gateway_loop` for gRPC-era
/// log-level push from Gateway).
pub(crate) async fn phase_d_run(
    ctx: &mut AgentBootContext,
    session_ctx: SessionBootContext,
    handles: SubsystemHandles,
    config: &RuntimeConfig,
    _log_reload_handle: Option<LogReloadHandle>,
) -> Result<()> {
    let _span = tracing::info_span!("startup_phase_d").entered();

    let SessionBootContext {
        session_manager,
        committed_lines: _committed_lines,
        idle_watcher,
    } = session_ctx;

    let SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    } = handles;

    // ADR-033: MQTT control → session dispatch channel.
    let (mqtt_dispatch_tx, mqtt_dispatch_rx) = tokio::sync::mpsc::unbounded_channel();

    // ADR-033: Forward Runtime HTTP dispatch messages to the MQTT dispatch channel.
    if let Some(http_rx) = ctx.http_dispatch_rx.take() {
        let tx = mqtt_dispatch_tx.clone();
        tokio::spawn(async move {
            let mut rx = http_rx;
            while let Some(msg) = rx.recv().await {
                let _ = tx.send(msg);
            }
            tracing::info!("Runtime HTTP dispatch channel closed");
        });
    }

    // ADR-034 §8 Phase 2-1: ControlAction → InboundMessage via single mapper.
    // Replaces the legacy 11-arm match that wrapped every command in
    // SystemNotification { notification_type: ... }.
    let _mqtt_handle = ctx.control_rx.take().map(|ctrl_rx| {
        let tx = mqtt_dispatch_tx.clone();
        // Phase B-3: idle watcher (if spawned) gets a `record_inbound()` on
        // every user action so its deadline resets. Clone-able handle,
        // so it's safe to share with this spawned task.
        let idle_watcher = idle_watcher.clone();
        tokio::spawn(async move {
            let mut rx = ctrl_rx;
            while let Some((topic, payload)) = rx.recv().await {
                match crate::mqtt::control_handler::parse_control_payload(&topic, &payload) {
                    Some(action) => {
                        if let Some((session_id, msg)) = control_action_to_inbound(action) {
                            // Reset the auto-sleep deadline on every parsed
                            // user action. `None` means the user chose
                            // "never sleep" — nothing to do.
                            if let Some(watcher) = idle_watcher.as_ref() {
                                watcher.record_inbound();
                            }
                            if tx.send((session_id, msg)).is_err() {
                                tracing::warn!(topic, "MQTT dispatch channel closed");
                            }
                        }
                    }
                    None => {
                        tracing::debug!(topic, "Failed to parse MQTT control payload");
                    }
                }
            }
        })
    });

    // ADR-034 §8 Phase 2-2: gRPC path removed. MQTT client is mandatory.
    if ctx.mqtt_client.is_none() {
        return Err(crate::error::RuntimeError::Config(
            "Phase D entered without MQTT client (gRPC path removed per ADR-034 §8 Phase 2)"
                .into(),
        ));
    }

    tracing::info!("All subsystems ready, announcing via MQTT");
    // Lifecycle publisher: used by dispatch_inbound to push SessionCreated /
    // SessionDeleted events to the MQTT broker so the Desktop (and any other
    // subscriber) can update its session list without polling. Cloned cheaply.
    let lifecycle_publisher: crate::mqtt::MqttChunkPublisher = if let Some(ref mqtt) =
        ctx.mqtt_client
    {
        let _ = mqtt.publish_status(true).await;
        tracing::info!(
            "Agent status published via MQTT for agent={}",
            ctx.agent_id
        );
        crate::mqtt::MqttChunkPublisher::from_runtime_client(mqtt)
    } else {
        // Unreachable: checked above.
        return Err(crate::error::RuntimeError::Config(
            "lifecycle publisher: MQTT client disappeared".into(),
        ));
    };

    let result = mqtt_only_loop(
        &session_manager,
        &lifecycle_publisher,
        mqtt_dispatch_rx,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
        ctx.mcp_notifier.subscribe(),
        ctx.identity_update_rx.take(),
        ctx.provider_update_rx.take(),
        ctx.search_update_rx.take(),
        &config.work_dir,
        ctx.session_config_slot.clone(),
    )
    .await;

    if let Some(handle) = chunk_relay {
        let _ = handle.await;
    }
    result
}

/// ADR-034 §8 Phase 2-1: ControlAction → InboundMessage single mapper.
///
/// Returns `Some((session_id, msg))` for every supported control command.
/// The mapper is exhaustive over `ControlAction` — adding a new variant
/// in `control_handler.rs` triggers a compile error here.
///
/// Session-bound commands return `Some((session_id, msg))` where
/// `session_id` is the proto field.  System-level commands (CreateSession,
/// IntentReceived) return `Some(("", msg))` to signal `mqtt_only_loop` to
/// route through `session_manager` (system-level) instead of a session task.
///
/// Unsupported / parse-failure returns `None` (caller logs).
fn control_action_to_inbound(
    action: crate::mqtt::control_handler::ControlAction,
) -> Option<(String, crate::agent::inbound::InboundMessage)> {
    use crate::agent::inbound::InboundMessage;
    use crate::mqtt::control_handler::ControlAction;

    match action {
        // ── Session lifecycle ──────────────────────────────────────────
        ControlAction::CreateSession => Some((
            String::new(),
            InboundMessage::CreateSession,
        )),
        ControlAction::DeleteSession { session_id } => Some((
            session_id.clone(),
            InboundMessage::DeleteSession { session_id },
        )),
        ControlAction::CloseSession { session_id } => {
            Some((session_id.clone(), InboundMessage::CloseSession { session_id }))
        }
        // ADR-038: explicit session activation. Routes through the system-level
        // dispatcher (empty session_id) because the OpenSession handler needs
        // to call `session_manager.open()` directly, not a specific SessionTask.
        ControlAction::OpenSession { session_id } => Some((
            String::new(),
            InboundMessage::OpenSession { session_id },
        )),
        ControlAction::UpdateSessionTitle { session_id, title } => Some((
            session_id.clone(),
            InboundMessage::UpdateSessionTitle { session_id, title },
        )),

        // ── Chat ────────────────────────────────────────────────────────
        ControlAction::SendMessage {
            session_id,
            message_id,
            content,
            command: _,
            params_json,
        } => Some((
            session_id,
            InboundMessage::ChatMessage {
                content,
                message_id,
                params_json,
            },
        )),
        ControlAction::StopGeneration {
            session_id,
            reason,
        } => Some((
            session_id,
            InboundMessage::Stop { reason },
        )),
        ControlAction::ContinueExecution { session_id, reason } => Some((
            session_id.clone(),
            InboundMessage::ContinueExecution { session_id, reason },
        )),
        // ADR-035 Phase 3: EnableNotify/DisableNotify removed from the
        // control-action → inbound-message mapping. The proto fields are
        // retained for wire compatibility but the runtime no longer acts
        // on them.

        // ── User responses ─────────────────────────────────────────────
        ControlAction::ApprovalDecision {
            session_id,
            request_id,
            approved,
            allow_all_session,
            reason,
        } => Some((
            session_id.clone(),
            InboundMessage::ApprovalDecision {
                session_id,
                request_id,
                approved,
                allow_all_session,
                // ADR-034: proto `string` is non-null but semantically optional.
                // Empty string is normalized to None to preserve gRPC-era semantics
                // where proto-empty = "no reason given".
                reason: if reason.is_empty() {
                    None
                } else {
                    Some(reason)
                },
            },
        )),
        ControlAction::QuestionAnswer {
            session_id,
            request_id,
            answer,
        } => Some((
            session_id.clone(),
            InboundMessage::QuestionAnswer {
                session_id,
                request_id,
                answer,
            },
        )),

        // ADR-045: Cancel a single in-flight tool. Routed via the fast
        // UserOperation channel so it takes effect even while the agent
        // loop is mid-iteration. Unknown tool_call_id is a no-op
        // (race vs. tool natural completion).
        ControlAction::CancelTool {
            session_id,
            tool_call_id,
        } => Some((
            session_id,
            InboundMessage::UserOperation(
                crate::agent::inbound::UserOp::CancelTool { tool_call_id },
            ),
        )),

        // ── Per-session config ─────────────────────────────────────────
        ControlAction::ModelSwitch {
            session_id,
            model_id,
            provider_id,
        } => Some((
            session_id,
            InboundMessage::ModelSwitchAction {
                model_id,
                provider_id,
            },
        )),
        ControlAction::ReasoningEffort { session_id, effort } => Some((
            session_id,
            InboundMessage::ReasoningEffortAction { effort },
        )),
        ControlAction::WorkspaceSwitch {
            session_id,
            workspace_id,
        } => Some((
            session_id,
            InboundMessage::WorkspaceSwitchAction { workspace_id },
        )),

        // ── Context management ─────────────────────────────────────────
        ControlAction::CompactContext { session_id } => Some((
            session_id,
            InboundMessage::CompactContextAction,
        )),
        ControlAction::CompressAction {
            session_id,
            compress_type,
        } => Some((
            session_id.clone(),
            InboundMessage::CompressAction {
                session_id,
                compress_type,
            },
        )),

        // ── System ─────────────────────────────────────────────────────
        ControlAction::IntentReceived {
            from,
            action,
            params_json,
        } => {
            let params: serde_json::Value =
                serde_json::from_str(&params_json).unwrap_or(serde_json::json!({}));
            Some((
                String::new(),
                InboundMessage::IntentMessage {
                    from,
                    action,
                    params,
                },
            ))
        }

        // ── Unsupported / parse failures (Phase 2-1: warn and drop) ───
        ControlAction::Unsupported { command_type } => {
            tracing::warn!(command_type, "Unsupported MQTT control command");
            None
        }
    }
}

/// ADR-033: MQTT-only gateway loop — no gRPC dependency.
///
/// Listens for MQTT control dispatch and MCP events. The actual
/// chat loop runs in session tasks; this loop just routes messages.
#[allow(clippy::too_many_arguments)]
async fn mqtt_only_loop(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    mut mqtt_dispatch_rx: tokio::sync::mpsc::UnboundedReceiver<(
        String,
        crate::agent::inbound::InboundMessage,
    )>,
    mut mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    >,
    mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_config_rx: tokio::sync::watch::Receiver<()>,
    // ADR-042: forwards `acowork/global/user_profile` retained updates from
    // the MQTT event loop to SessionManager. None when running in tests or
    // when MQTT is unavailable (Standalone mode).
    mut identity_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<acowork_core::protocol::UserProfile>,
    >,
    mut provider_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::ProviderUpdate>,
    >,
    mut search_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::SearchUpdate>,
    >,
    work_dir: &str,
    session_config_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
) -> Result<()> {
    tracing::info!("MQTT-only gateway loop started");
    let work_dir = std::path::PathBuf::from(work_dir);

    loop {
        tokio::select! {
            // ── ADR-034 §8 Phase 2-1: single dispatch table ────────────
            dispatch_result = mqtt_dispatch_rx.recv() => {
                match dispatch_result {
                    Some((session_id, msg)) => {
                        if let Err(e) = dispatch_inbound(
                            session_manager,
                            lifecycle_publisher,
                            session_id,
                            msg,
                            &work_dir,
                            &session_config_slot,
                        ).await {
                            tracing::warn!(error = %e, "MQTT dispatch failed");
                        }
                    }
                    None => break, // channel closed
                }
            }

            // Initial MCP auto-connect result
            mcp_result = async {
                match &mut mcp_startup_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some((registry, wrappers, specs, failures)) = mcp_result {
                    session_manager.lock().await.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                }
                mcp_startup_rx = None;
            }

            // Runtime MCP connect result
            mcp_runtime_result = mcp_runtime_rx.recv() => {
                if let Some((registry, wrappers, specs, failures)) = mcp_runtime_result {
                    session_manager.lock().await.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                }
            }

            // MCP config change notification
            _ = mcp_config_rx.changed() => {
                tracing::info!("MCP config change — reconnecting MCP servers (background)");
                let merged = crate::agent_config::load_active_mcp_configs(
                    std::path::Path::new(&work_dir),
                );
                let tx = mcp_runtime_tx.clone();
                tokio::spawn(async move {
                    let (registry, failures) =
                        acowork_mcp::client::McpRegistry::connect_all(&merged)
                            .await
                            .expect("connect_all is non-fatal and should never fail");
                    let registry = std::sync::Arc::new(registry);
                    let mut wrappers = Vec::new();
                    let mut specs = Vec::new();
                    for prefixed_name in registry.tool_names() {
                        if let Some(def) = registry.get_tool_def(&prefixed_name) {
                            let wrapper = acowork_mcp::wrapper::McpToolWrapper::new(
                                prefixed_name.clone(),
                                def,
                                registry.clone(),
                            );
                            use acowork_core::tools::traits::Tool;
                            let tool_spec = wrapper.spec();
                            let serialized = serde_json::to_value(&tool_spec).unwrap_or_default();
                            specs.push((tool_spec.name.clone(), serialized));
                            wrappers.push(wrapper);
                        }
                    }
                    let _ = tx.send((registry, wrappers, specs, failures)).await;
                });
            }

            // ADR-042: `acowork/global/user_profile` retained update →
            // SessionManager::update_user_identity → broadcast to all sessions.
            identity = async {
                match identity_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(profile) = identity {
                    tracing::info!(
                        user_id = %profile.user_id,
                        language = %profile.language,
                        "Applying acowork/global/user_profile update to SessionManager"
                    );
                    session_manager.lock().await.update_user_identity(Some(profile));
                }
            }

            // `acowork/global/providers` retained update →
            // SessionManager::update_global_provider_list → broadcast to all sessions.
            provider = async {
                match provider_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = provider {
                    tracing::info!(
                        provider_count = update.provider_list.len(),
                        version = update.provider_list_version,
                        key_count = update.provider_key_vault.len(),
                        "Applying acowork/global/providers update to SessionManager"
                    );
                    session_manager.lock().await.update_global_provider_list(
                        update.provider_list,
                        update.provider_list_version,
                        update.provider_key_vault,
                    );
                }
            }

            // `acowork/global/searches` retained update →
            // SessionManager::update_search_config → broadcast to all sessions.
            search = async {
                match search_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = search {
                    tracing::info!(
                        search_count = update.search_list.len(),
                        key_count = update.search_key_vault.len(),
                        "Applying acowork/global/searches update to SessionManager"
                    );
                    session_manager.lock().await.update_search_config(
                        update.search_key_vault,
                        update.search_list,
                    );
                }
            }
        }
    }

    tracing::info!("MQTT-only gateway loop ended");
    Ok(())
}

/// ADR-034 §8 Phase 2-1: single dispatch table for `InboundMessage`.
///
/// One arm per `InboundMessage` variant — no notification_type string
/// parsing, no SystemNotification re-mapping for the 8 new control commands.
///
/// Routes:
/// - `UserMessage` / `Stop` / `ContinueExecution` / `ApprovalDecision` /
///   `QuestionAnswer` / `UserOperation` / `IntentMessage` → session task inbox
/// - `CloseSession` → `SessionManager::delete_session`
/// - `UpdateSessionTitle` → `SessionMessage::UpdateSessionTitle` (direct,
///   does NOT wrap in `SystemNotification` — ADR-034 §7.1 G1 fix)
/// - `EnableNotify` / `DisableNotify` → `SessionMessage::EnableNotify` /
///   `SessionMessage::DisableNotify` (the existing session_task handler
///   sets `session_core.notify_enabled` AtomicBool)
/// - `CompressAction` → `SessionMessage::CompressAction(CompressionAction)`
///   with explicit CompressType i32 → CompressionAction mapping
///   (1=SUMMARY → CompressSummary; ADR-052 removed TOOL_RESULTS because
///   tool-result compression is now LLM-initiated via `context_abandon`)
/// - `SystemNotification` → legacy fallback (Phase 7: no longer produced by control path)
async fn dispatch_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: String,
    msg: crate::agent::inbound::InboundMessage,
    work_dir: &std::path::Path,
    session_config_slot: &Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
) -> crate::error::Result<()> {
    use crate::agent::inbound::InboundMessage;
    use crate::agent::loop_::CompressionAction;
    use crate::agent::session::SessionMessage;
    use crate::cancellation::{CancellationReason, StopSource};
    use crate::error::RuntimeError;
    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};

    // ── System-level (session_id empty) ─────────────────────────────
    if session_id.is_empty() {
        return match msg {
            InboundMessage::CreateSession => {
                // Phase A fix: route through `create_frontend_session` so the
                // session gets a JSONL file, per-session meta, and enable_notify —
                // not just an in-memory spawn. Without this the Desktop never
                // sees the new session via fetchSessions and the session has no
                // persisted workspace context.
                match session_manager.lock().await.create_frontend_session(None, None, None).await {
                    Ok(sid) => {
                        tracing::info!(new_sid = %sid, "MQTT: session created via control command");
                        // Publish SessionCreated to the lifecycle topic so the
                        // Desktop updates its session list immediately.
                        let created = data_envelope::Payload::SessionCreated(
                            acowork_core::mqtt_proto::SessionCreated {
                                agent_id: lifecycle_publisher.agent_id().to_string(),
                                session_id: sid.clone(),
                                title: String::new(),
                                created_at: chrono::Utc::now().to_rfc3339(),
                            },
                        );
                        let envelope = DataEnvelope {
                            version: 1,
                            payload: Some(created),
                        };
                        if let Err(e) = lifecycle_publisher
                            .publish_lifecycle("created", &envelope)
                            .await
                        {
                            tracing::warn!(
                                session_id = %sid,
                                error = %e,
                                "Failed to publish SessionCreated lifecycle event"
                            );
                        }
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "MQTT: create_session failed");
                        Err(e)
                    }
                }
            }
            InboundMessage::IntentMessage { from, action, .. } => {
                tracing::warn!(
                    from = %from,
                    action = %action,
                    "IntentMessage without target session — dropped (MQTT dispatch expects session_id)"
                );
                Err(RuntimeError::Config(
                    "IntentMessage without target session".to_string(),
                ))
            }
            // ADR-038: explicit session activation. Routed through the
            // system-level branch because `session_manager.lock().await.open()` is a
            // manager-level operation, not a session-task-level one.
            InboundMessage::OpenSession { session_id } => {
                if session_id.is_empty() {
                    return Err(RuntimeError::Config(
                        "OpenSession requires non-empty session_id".to_string(),
                    ));
                }
                handle_open_session(session_manager, lifecycle_publisher, &session_id, work_dir).await
            }
            // ── ADR-052 §3.5: agent-level config mutations are
            //    GLOBAL, not per-session. Route through SessionManager
            //    so the shared template, the runtime_overrides cache,
            //    every active SessionTask's ContextBuilder, and every
            //    mid-execution AgentLoop all see the change in one
            //    shot. The session-level arm below mirrors this for
            //    defensive uniformity (any per-session producer gets
            //    the same global policy).
            //
            //    The HTTP layer (`http::server::dispatch_agent_level_config`)
            //    produces system-level messages; MQTT producers should
            //    too. Either way, the policy at this table is
            //    "config mutations are agent-scoped".
            InboundMessage::UserOperation(
                crate::agent::inbound::UserOp::UpdateRuntimeConfig(overrides),
            ) => {
                let failed = session_manager.lock().await.apply_runtime_config_override(&overrides);
                if !failed.is_empty() {
                    tracing::warn!(
                        failed_sessions = failed.len(),
                        "system-level UpdateRuntimeConfig: some sessions missed the broadcast (likely closed)"
                    );
                }
                Ok(())
            }
            InboundMessage::UpdateBuiltinTools { entries } => {
                session_manager.lock().await.apply_builtin_tools_enabled(&entries);
                Ok(())
            }
            other => Err(RuntimeError::Config(format!(
                "system-level dispatch: unsupported variant {:?}",
                std::mem::discriminant(&other)
            ))),
        };
    }

    // ── Session-level: single dispatch table ─────────────────────────
    match msg {
        // ① User chat message → session task inbox
        InboundMessage::UserMessage(text) => forward_to_session_inbound(
            session_manager,
            lifecycle_publisher,
            &session_id,
            "user_message",
            work_dir,
            InboundMessage::UserMessage(text),
        ).await,

        // ② Stop signal → session task inbox
        InboundMessage::Stop { reason } => {
            // ADR-044 §4.5: flip the session's **current request's** `CancelHandle`
            // *before* forwarding so any currently-blocked `tokio::select!`
            // branch on the session wakes immediately. The handle's level-
            // triggered `Notify` + `AtomicU8` state means the cancel takes
            // effect on the next checkpoint even if the session task is
            // mid-await on something *other* than a Notify — notably inside
            // `provider.chat_stream().await` while establishing a TCP/TLS
            // connection (the TTFT stop bug, ADR §1.3, §4.4).
            //
            // `session_manager.lock().await.cancel_handle(&session_id)` reads through the
            // `Arc<parking_lot::Mutex<CancelHandle>>` slot, so we always
            // target the *current* request's generation — never a stale
            // clone from session creation time (the §4.5 guarantee).
            //
            // We deliberately call `cancel()` regardless of whether the
            // session is currently registered in `cancel_handles`:
            // `None` simply means the session has already been evicted or
            // closed, in which case the cancel is a no-op (no panic, just a
            // debug log) and the subsequent `forward_to_session_inbound`
            // call will surface the same eviction as a structured error.
            // NOTE(deadlock): the cancel handle and the agent_id MUST be
            // hoisted out of the `match` scrutinee. The scrutinee temporary
            // (`MutexGuard`) lives until the end of the whole `match`
            // expression, so re-locking `session_manager` inside a match
            // arm self-deadlocks on the non-reentrant `tokio::sync::Mutex`.
            // This exact bug froze the entire MQTT control plane (Stop /
            // CreateSession / ChatMessage all silently queued) while the
            // session kept running - see the 2026-08-20 incident.
            let agent_id = session_manager.lock().await.agent_id().to_string();
            let cancel_handle = session_manager.lock().await.cancel_handle(&session_id);
            match cancel_handle {
                Some(handle) => {
                    handle.cancel(CancellationReason::UserStop {
                        source: StopSource::ChatPanel {
                            agent_id,
                            session_id: session_id.clone(),
                        },
                        reason: reason.clone(),
                    });
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "ADR-044 §4.5: cancellation handle fired for MQTT Stop signal"
                    );
                }
                None => {
                    tracing::debug!(
                        session_id = %session_id,
                        "Stop signal: no cancel handle registered (session evicted?)"
                    );
                }
            }
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "stop",
                work_dir,
                InboundMessage::Stop { reason },
            ).await
        }

        // ③ Continue execution → session task inbox
        InboundMessage::ContinueExecution { reason, .. } => forward_to_session_inbound(
            session_manager,
            lifecycle_publisher,
            &session_id,
            "continue_execution",
            work_dir,
            InboundMessage::ContinueExecution {
                session_id: session_id.clone(),
                reason,
            },
        ).await,

        // ④ Approval decision → session task inbox
        InboundMessage::ApprovalDecision {
            request_id,
            approved,
            allow_all_session,
            reason,
            ..
        } => forward_to_session_inbound(
            session_manager,
            lifecycle_publisher,
            &session_id,
            "approval_decision",
            work_dir,
            InboundMessage::ApprovalDecision {
                session_id: session_id.clone(),
                request_id,
                approved,
                allow_all_session,
                reason,
            },
        ).await,

        // ⑤ Question answer → session task inbox
        InboundMessage::QuestionAnswer {
            request_id, answer, ..
        } => forward_to_session_inbound(
            session_manager,
            lifecycle_publisher,
            &session_id,
            "question_answer",
            work_dir,
            InboundMessage::QuestionAnswer {
                session_id: session_id.clone(),
                request_id,
                answer,
            },
        ).await,

        // ⑥ UserOperation (StopLoop, ContinueLoop, ApprovalDecision, QuestionAnswer)
        //
        // `UpdateRuntimeConfig` is an agent-level config mutation and is
        // routed globally through `SessionManager::apply_runtime_config_override`
        // (see the system-level arm above for the canonical path; this
        // arm is defensive uniformity - any per-session producer
        // targeting a single session_id gets the same global policy
        // because config mutations are agent-scoped).
        InboundMessage::UserOperation(op) => match op {
            crate::agent::inbound::UserOp::UpdateRuntimeConfig(overrides) => {
                let failed = session_manager.lock().await.apply_runtime_config_override(&overrides);
                if !failed.is_empty() {
                    tracing::warn!(
                        failed_sessions = failed.len(),
                        "per-session UpdateRuntimeConfig: some sessions missed the broadcast (likely closed)"
                    );
                }
                Ok(())
            }
            _ => forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "user_operation",
                work_dir,
                InboundMessage::UserOperation(op),
            ).await,
        },

        // ⑦ IntentMessage → session task inbox
        InboundMessage::IntentMessage {
            from,
            action,
            params,
        } => forward_to_session_inbound(
            session_manager,
            lifecycle_publisher,
            &session_id,
            "intent",
            work_dir,
            InboundMessage::IntentMessage { from, action, params },
        ).await,

        // ── ADR-034 §8 Phase 2: 8 new control commands ────────────────

        // ⑧ CloseSession → SessionManager::close_session (preserves JSONL, triggers
        // distillation). Publishes SessionDeleted so the Desktop can prune its
        // session list immediately.
        //
        // This used to call delete_session — a Phase 6 gRPC-cleanup regression
        // that physically removed the JSONL/meta files and made the closed
        // session disappear from fetchSessions. Bug 2/Bug 3 root cause.
        InboundMessage::CloseSession { session_id: sid } => {
            let result = session_manager.lock().await.close_session(&sid).await;
            // Publish SessionDeleted regardless of close result so the Desktop
            // prunes its UI list. close_session returns Err if the session was
            // already gone — that's still a "gone" signal for the Desktop.
            let deleted = data_envelope::Payload::SessionDeleted(
                acowork_core::mqtt_proto::SessionDeleted {
                    agent_id: lifecycle_publisher.agent_id().to_string(),
                    session_id: sid.clone(),
                    deleted_at: chrono::Utc::now().to_rfc3339(),
                },
            );
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(deleted),
            };
            if let Err(e) = lifecycle_publisher
                .publish_lifecycle("deleted", &envelope)
                .await
            {
                tracing::warn!(
                    session_id = %sid,
                    error = %e,
                    "Failed to publish SessionDeleted lifecycle event (close)"
                );
            }
            // Log but don't propagate close errors (idempotent close is the
            // best UX — re-closing an already-closed session should not break
            // the dispatch loop).
            if let Err(e) = result {
                tracing::warn!(session_id = %sid, error = %e, "close_session reported error (ignored)");
            }
            Ok(())
        }

        // ⑨ UpdateSessionTitle — direct SessionMessage dispatch (Phase 2-6).
        // Replaces the gRPC-era SystemNotification detour (fixes §7.1 G1).
        // session_task.rs handles SessionMessage::UpdateSessionTitle at line ~1344.
        InboundMessage::UpdateSessionTitle { title, .. } => session_manager
            .lock().await.send_to_session(&session_id, SessionMessage::UpdateSessionTitle { title })
            .map_err(|e| RuntimeError::Config(format!("UpdateSessionTitle: {}", e))),

        // ADR-035 Phase 3: ⑩/⑪ EnableNotify/DisableNotify removed — push
        // drives all streaming, no front/back suppression. InboundMessage
        // variants removed; proto fields retained for wire compat.

        // ⑫ CompressAction — explicit CompressType i32 → CompressionAction mapping
        // (Phase 2-7: two paths must not cross).
        // CompressType::SUMMARY (1)     → CompressionAction::CompressSummary
        // Anything else is rejected (forwarded to session_task which emits an error).
        InboundMessage::CompressAction {
            compress_type, ..
        } => {
            let action = match compress_type {
                1 => CompressionAction::CompressSummary,
                other => {
                    return Err(RuntimeError::Config(format!(
                        "CompressAction: invalid compress_type {} (expected 1=SUMMARY or 2=TOOL_RESULTS)",
                        other
                    )));
                }
            };
            session_manager
                .lock().await.send_to_session(&session_id, SessionMessage::CompressAction(action))
                .map_err(|e| RuntimeError::Config(format!("CompressAction: {}", e)))
        }

        // ⑫b UpdateBuiltinTools - ADR-029/ADR-052: agent-level builtin tool
        // enabled mutation. Routed globally through
        // `SessionManager::apply_builtin_tools_enabled` so sessions
        // created AFTER this PUT inherit the new enabled flags (the
        // template is CoW-synced) and every active session's
        // dispatch list + LLM tool_definitions are rebuilt atomically.
        // See the system-level arm above for the canonical routing;
        // this is defensive uniformity.
        InboundMessage::UpdateBuiltinTools { entries } => {
            session_manager.lock().await.apply_builtin_tools_enabled(&entries);
            Ok(())
        }

        // ⑬ CreateSession (session-level DeleteSession)
        InboundMessage::DeleteSession { session_id: sid } => {
            session_manager.lock().await.delete_session(&sid).await;
            // Notify the Desktop so it can prune its session list immediately.
            let deleted = data_envelope::Payload::SessionDeleted(
                acowork_core::mqtt_proto::SessionDeleted {
                    agent_id: lifecycle_publisher.agent_id().to_string(),
                    session_id: sid.clone(),
                    deleted_at: chrono::Utc::now().to_rfc3339(),
                },
            );
            let envelope = DataEnvelope {
                version: 1,
                payload: Some(deleted),
            };
            if let Err(e) = lifecycle_publisher
                .publish_lifecycle("deleted", &envelope)
                .await
            {
                tracing::warn!(
                    session_id = %sid,
                    error = %e,
                    "Failed to publish SessionDeleted lifecycle event (delete)"
                );
            }
            Ok(())
        }

        // ⑭ ChatMessage → SessionMessage::ChatMessage (from MQTT SendMessage)
        InboundMessage::ChatMessage {
            content,
            message_id,
            params_json,
        } => {
            // Parse params_json to extract skill_instructions and
            // attached_items (ADR-046 replaces the prior document_ids +
            // content_parts + attached_context fields).
            //
            // ADR-046: `attached_items` is a strongly-typed discriminated
            // union array. Each item carries a `"type"` tag matching the
            // `AttachedItem` serde enum. Legacy `document_ids` and
            // `attached_context` from old desktop clients are NOT accepted
            // — there is no compatibility layer per ADR §4.
            let mut skill_instructions: Option<String> = None;
            let mut attached_items: Option<Vec<acowork_core::protocol::AttachedItem>> = None;
            let mut content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>> = None;

            if !params_json.is_empty()
                && let Ok(params) = serde_json::from_str::<serde_json::Value>(&params_json)
            {
                    if let Some(si) = params.get("skill_instructions").and_then(|v| v.as_str())
                        && !si.is_empty()
                    {
                        skill_instructions = Some(si.to_string());
                    }
                    if let Some(items) = params.get("attached_items").and_then(|v| v.as_array()) {
                        let parsed: Vec<acowork_core::protocol::AttachedItem> = items
                            .iter()
                            .filter_map(|d| serde_json::from_value::<acowork_core::protocol::AttachedItem>(d.clone()).ok())
                            .collect();
                        if !parsed.is_empty() {
                            attached_items = Some(parsed);
                        }
                    }
                    if let Some(parts) = params.get("content_parts").and_then(|v| v.as_array()) {
                        let parsed: Vec<acowork_core::providers::traits::ContentPart> = parts
                            .iter()
                            .filter_map(|p| serde_json::from_value(p.clone()).ok())
                            .collect();
                        if !parsed.is_empty() {
                            content_parts = Some(parsed);
                        }
                    }
            }

            session_manager
                .lock().await.send_to_session(
                    &session_id,
                    SessionMessage::ChatMessage {
                        content,
                        message_id,
                        skill_instructions,
                        attached_items,
                        content_parts,
                    },
                )
                .map_err(|e| RuntimeError::Config(format!("ChatMessage: {}", e)))
        }

        // ⑮ ModelSwitchAction → SessionManager::route_model_switch
        InboundMessage::ModelSwitchAction {
            model_id,
            provider_id,
        } => {
            let delta = crate::agent::session_config::SessionConfigDelta {
                model: Some(model_id),
                provider: provider_id,
                ..Default::default()
            };
            let slot = session_config_slot.lock().await;
            if let Some(ref svc) = *slot {
                svc.apply_config(&session_id, delta)
                    .await
                    .map_err(|e| RuntimeError::Config(format!("ModelSwitchAction: {}", e)))
            } else {
                session_manager
                    .lock().await.route_model_switch(&session_id, delta.model.unwrap_or_default(), delta.provider)
                    .map_err(|e| RuntimeError::Config(format!("ModelSwitchAction (fallback): {}", e)))
            }
        }

        // ⑯ ReasoningEffortAction → SessionManager::route_reasoning_effort
        InboundMessage::ReasoningEffortAction { effort } => {
            let delta = crate::agent::session_config::SessionConfigDelta {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            let slot = session_config_slot.lock().await;
            if let Some(ref svc) = *slot {
                svc.apply_config(&session_id, delta)
                    .await
                    .map_err(|e| RuntimeError::Config(format!("ReasoningEffortAction: {}", e)))
            } else {
                session_manager
                    .lock().await.route_reasoning_effort(&session_id, delta.reasoning_effort.unwrap_or_default())
                    .map_err(|e| RuntimeError::Config(format!("ReasoningEffortAction (fallback): {}", e)))
            }
        }

        // ⑰ WorkspaceSwitchAction → SessionManager::route_workspace_switch
        InboundMessage::WorkspaceSwitchAction { workspace_id } => {
            session_manager.lock().await.route_workspace_switch(&session_id, &workspace_id);
            Ok(())
        }

        // ⑱ CompactContextAction → SessionMessage::CompactContext
        InboundMessage::CompactContextAction => session_manager
            .lock().await.send_to_session(&session_id, SessionMessage::CompactContext)
            .map_err(|e| RuntimeError::Config(format!("CompactContextAction: {}", e))),

        // ── Legacy fallback (Phase 7: no longer produced by control path) ──
        InboundMessage::SystemNotification {
            notification_type,
            ..
        } => {
            tracing::warn!(
                notification_type,
                "SystemNotification received at session level — no longer expected (Phase 7 cleanup)"
            );
            Ok(())
        }

        // CreateSession at session level is a no-op (only meaningful at system level)
        InboundMessage::CreateSession => {
            tracing::warn!(
                "CreateSession received at session level — ignoring (system-level only)"
            );
            Ok(())
        }

        // ADR-038: OpenSession is a system-level command (handled by
        // `handle_open_session` above). Reaching here means a non-empty
        // session_id was carried through control_action_to_inbound with
        // empty session_id, which is a routing bug. Log loudly and no-op.
        InboundMessage::OpenSession { session_id } => {
            tracing::error!(
                session_id = %session_id,
                "OpenSession reached session-level dispatch — routing bug, ignored"
            );
            Ok(())
        }
    }
}

/// ADR-038: Handle `InboundMessage::OpenSession`.
///
/// Transitions Closed/NotFound → Active (idempotent for Active). Always
/// publishes a `SessionOpened` ack on success. On failure publishes
/// `SessionNotOpened` (instead of returning an error) so the frontend gets
/// a structured event it can react to.
async fn handle_open_session(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: &str,
    work_dir: &std::path::Path,
) -> crate::error::Result<()> {
    use crate::agent::session::{SessionLifecycleState, SessionOpenOutcome};

    let state = session_manager.lock().await.get_lifecycle_state(session_id, work_dir);
    let result = match state {
        SessionLifecycleState::NotFound => {
            // Surface as a structured event so the frontend can react.
            let _ = lifecycle_publisher
                .publish_session_not_opened(session_id, "open_session", "session_not_found")
                .await;
            tracing::info!(
                session_id = %session_id,
                "OpenSession: session not found on disk"
            );
            return Ok(());
        }
        SessionLifecycleState::Active => {
            // Already in memory; idempotent success.
            Ok(SessionOpenOutcome::AlreadyActive)
        }
        SessionLifecycleState::Closed => session_manager.lock().await.open(session_id, work_dir).await,
    };

    match result {
        Ok(outcome) => {
            let status = match outcome {
                SessionOpenOutcome::AlreadyActive => "already_active",
                SessionOpenOutcome::ResumedFromDisk => "resumed_from_disk",
            };
            let (model, provider, last_active_at) =
                session_manager.lock().await.session_metadata_summary(session_id, work_dir);
            if let Err(e) = lifecycle_publisher
                .publish_session_opened(session_id, status, model, provider, last_active_at)
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    status = %status,
                    error = %e,
                    "OpenSession: failed to publish SessionOpened ack"
                );
            }
            tracing::info!(
                session_id = %session_id,
                status = %status,
                "OpenSession: success"
            );
            Ok(())
        }
        Err(e) => {
            let _ = lifecycle_publisher
                .publish_session_not_opened(session_id, "open_session", "session_closed")
                .await;
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "OpenSession: failed to resume session"
            );
            Ok(())
        }
    }
}

/// Forward an `InboundMessage` to the session task's `agent_inbound_tx`.
///
/// ADR-038: when the target session is not Active (Closed or NotFound
/// in `SessionManager`), we return an error AND publish a structured
/// `SessionNotOpened` event so the Desktop can surface a reopen
/// affordance. Without this, a frontend that forgot to send
/// `open_session` first would silently drop the message — the bug we
/// are fixing here.
async fn forward_to_session_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: &str,
    attempted_command: &str,
    work_dir: &std::path::Path,
    msg: crate::agent::inbound::InboundMessage,
) -> crate::error::Result<()> {
    use crate::error::RuntimeError;
    // Hold the SessionManager lock for the whole body: `SessionHandle`
    // is borrowed from the manager (no Clone impl), and `send_inbound`
    // only does `touch()` + `try_send()` — neither touches the manager,
    // so keeping the lock here is safe and avoids a re-borrow.
    let manager_guard = session_manager.lock().await;
    let handle = match manager_guard.get_session(session_id) {
        Some(h) => h,
        None => {
            // Determine reason: Closed (file exists on disk) vs NotFound
            // (no file) — the Desktop uses this to render the right toast.
            let reason = match manager_guard.get_lifecycle_state(session_id, work_dir) {
                crate::agent::session::SessionLifecycleState::Closed => "session_closed",
                _ => "session_not_found",
            };
            tracing::warn!(
                session_id = %session_id,
                attempted_command = %attempted_command,
                reason = %reason,
                "session not Active: forwarding SessionNotOpened",
            );
            // Best-effort publish — never block on the broker.
            let publisher = lifecycle_publisher.clone();
            let sid = session_id.to_string();
            let cmd = attempted_command.to_string();
            let reason_owned = reason.to_string();
            tokio::spawn(async move {
                let _ = publisher
                    .publish_session_not_opened(&sid, &cmd, &reason_owned)
                    .await;
            });
            return Err(RuntimeError::Config(format!(
                "session not Active ({}): {}",
                reason, session_id,
            )));
        }
    };
    handle
        .send_inbound(msg)
        .map_err(|e| RuntimeError::Config(format!("send_inbound failed: {}", e)))
}

