//! Phase C: spawn subsystems.
//!
//! Covers the latter part of Step 9:
//!   - Spawn chunk_relay task first so the chunk channel is draining
//!   - DevMode: start Debug Protocol server if --dev-mode
//!   - Sync agent_mcp.json catalog from Gateway hello
//!   - Spawn MCP auto-connect background task
//!   - Send workspace config snapshot to Gateway

use std::sync::Arc;

use crate::config::RuntimeConfig;
use crate::conversation::{ConfigChange, ConversationSession, StateChange};
use crate::error::Result;
use crate::http::SharedAgentCore;
use crate::startup::context::{AgentBootContext, SessionBootContext};
use acowork_core::mqtt_proto::StreamLine;

/// Resources produced by Phase C, needed by Phase D.
pub(crate) struct SubsystemHandles {
    /// chunk_relay task join handle (Gateway mode only).
    pub chunk_relay: Option<tokio::task::JoinHandle<()>>,
    /// MCP startup result receiver (Gateway mode only).
    pub mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    >,
    /// Runtime MCP channel used by run_gateway_loop.
    pub mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    pub mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
}

/// Phase C: spawn background subsystems (Gateway mode).
///
/// After this phase the agent is functionally ready:
/// - chunk_relay is running and draining the chunk channel
/// - MCP auto-connect is progressing in the background
pub(crate) async fn phase_c_spawn_subsystems(
    ctx: &mut AgentBootContext,
    session_ctx: &mut SessionBootContext,
    config: &RuntimeConfig,
) -> Result<SubsystemHandles> {
    let _span = tracing::info_span!("startup_phase_c").entered();

    let work_dir_path = std::path::Path::new(&config.work_dir);

    // ── Spawn chunk relay task first ─────────────────────────────────
    // This must run before AgentReady is sent so the chunk channel is
    // already being drained when the Gateway loop starts.
    //
    // ADR-033: MQTT chunk relay takes priority over gRPC when MQTT is
    // available. All chunk events are published to the MQTT broker as
    // `DataEnvelope { payload: SessionMessage }` protobuf on session
    // message topics. Desktop subscribes directly to the broker.
    let agent_id_for_relay = ctx.agent_id.clone();
    let chunk_relay = if ctx.chunk_rx.is_some() {
        if let Some(ref mqtt_client) = ctx.mqtt_client {
            // MQTT chunk relay — publish all events via MQTT broker.
            let chunk_rx = ctx.chunk_rx.take().unwrap();
            let chunk_publisher = crate::mqtt::MqttChunkPublisher::from_runtime_client(mqtt_client);
            Some(tokio::spawn(async move {
                tracing::info!("MQTT chunk relay started");
                let mut chunk_rx = chunk_rx;
                while let Some(session_event) = chunk_rx.recv().await {
                    relay_chunk_event_mqtt(
                        &chunk_publisher,
                        &agent_id_for_relay,
                        &session_event.session_id,
                        session_event.event,
                    )
                    .await;
                }
                tracing::debug!("MQTT chunk relay task ended");
            }))
        } else {
            None
        }
    } else {
        None
    };

    // ── DevMode: register Debug HTTP routes + spawn events publisher ──
    if config.dev_mode {
        let debug_port = config.debug_port as u32;
        tracing::info!(
            debug_port = debug_port,
            "DevMode enabled at startup — registering Debug Protocol routes and MQTT events publisher"
        );
        // ADR-048: pass the MQTT client so the debug events publisher
        // forwards every TaggedEvent to the broker on
        // `acowork/agents/{id}/debug/events/{type}`. The publisher
        // needs `Arc<RuntimeMqttClient>`; clone the cheap (internally
        // Arc-wrapped) client and re-wrap it.
        let mqtt_client = ctx.mqtt_client.clone().map(std::sync::Arc::new);
        session_ctx
            .session_manager
            .lock()
            .await
            .enable_debug_mode(debug_port, mqtt_client)
            .await;

        // ADR-048: populate the HTTP server's late-bind Debug service
        // slot. Without this, /api/debug/* routes return 503 forever.
        // The service was built inside `enable_debug_mode` after
        // per-session controllers and senders were registered.
        if let Some(svc) = session_ctx
            .session_manager
            .lock()
            .await
            .debug_service()
        {
            let svc_dyn: Arc<dyn crate::usecases::DebugService> = svc;
            *ctx.debug_service_slot.lock().await = Some(svc_dyn);
            tracing::info!("Debug service slot populated (DevMode active)");
        } else {
            tracing::warn!(
                "DevMode enabled but debug_service is None — /api/debug/* will return 503"
            );
        }
    }

    // ADR-040: gRPC hello_config MCP catalog sync removed.
    // MCP servers are loaded from the on-disk agent_mcp.json config.

    // ── MCP auto-connect at startup (background, non-blocking) ───────
    let mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    > = {
        let mcp_configs = crate::agent_config::load_active_mcp_configs(work_dir_path);
        if !mcp_configs.is_empty() {
            let (tx, rx) =
                tokio::sync::mpsc::channel::<crate::tools::mcp_manager::McpConnectResult>(1);
            tracing::info!(
                mcp_count = mcp_configs.len(),
                "Auto-connecting to persisted MCP servers at startup (background)"
            );
            tokio::spawn(async move {
                let (registry, failures) =
                    acowork_mcp::client::McpRegistry::connect_all(&mcp_configs)
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
            Some(rx)
        } else {
            None
        }
    };

    let (mcp_runtime_tx, mcp_runtime_rx) =
        tokio::sync::mpsc::channel::<crate::tools::mcp_manager::McpConnectResult>(1);

    Ok(SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    })
}

/// MQTT-based chunk relay — publishes each ChunkEvent to the broker.
///
/// All events are encoded as `DataEnvelope { payload: SessionMessage }`
/// protobuf and published to `acowork/agents/{id}/sessions/{sid}/messages/{event_type}`.
/// Desktop subscribes to these topics directly on the broker.
///
/// **Ordering invariant**: each publish is awaited before the next
/// ChunkEvent is consumed. This makes the relay's call order the same as
/// the broker's enqueue order, eliminating the previous `tokio::spawn` race
/// between events of asymmetric payload size (notably
/// `session_state_changed(idle)` racing the last `record_complete`).
async fn relay_chunk_event_mqtt(
    publisher: &crate::mqtt::MqttChunkPublisher,
    _agent_id: &str,
    sid: &str,
    event: crate::agent::loop_::ChunkEvent,
) {
    use crate::agent::loop_::ChunkEvent;

    match event {
        ChunkEvent::ContextUsage(ctx_info) => {
            // Publish the full ContextUsageInfo so the StatusBar can render
            // total_tokens / context_window / usage_percent, not just the
            // last-turn input/output tokens.
            publisher.publish_context_usage(sid, &ctx_info).await;
        }

        ChunkEvent::CompactingStarted => {
            publisher.publish_compacting(sid, true).await;
        }

        ChunkEvent::CompactingEnded => {
            publisher.publish_compacting(sid, false).await;
        }

        ChunkEvent::IterationLimitPaused {
            iteration,
            max_iterations,
            message,
        } => {
            publisher
                .publish_iteration_limit_paused(sid, iteration, max_iterations, message)
                .await;
        }

        ChunkEvent::ToolApprovalNeeded {
            request_id,
            tool_name,
            action,
            risk_level,
            reason,
            tool_call_id,
            approval_timeout_secs,
        } => {
            publisher
                .publish_tool_approval_needed(
                    crate::mqtt::client::ToolApprovalNeededEvent {
                        session_id: sid,
                        request_id: &request_id,
                        tool_name: &tool_name,
                        action: &action,
                        risk_level: &risk_level,
                        reason: &reason,
                        tool_call_id: &tool_call_id,
                        approval_timeout_secs,
                    },
                )
                .await;
        }

        ChunkEvent::Done {
            message_id, ..
        } => {
            publisher.publish_done(sid, &message_id).await;
        }

        ChunkEvent::Error {
            user_message,
            detail: _,
            error_type: _,
            message_id,
        } => {
            publisher.publish_error(sid, &message_id, &user_message).await;
        }

        ChunkEvent::Stopped { .. } => {
            // Use empty message_id for stopped — the event itself is the signal.
            publisher.publish_stopped(sid, "").await;
        }

        ChunkEvent::SessionStateChanged { state } => {
            publisher.publish_session_state(sid, &state).await;
        }

        ChunkEvent::TodoListUpdated { todos } => {
            let todos_json = serde_json::to_string(&todos).unwrap_or_default();
            publisher.publish_todo_updated(sid, &todos_json).await;
        }

        ChunkEvent::NewDataAvailable {
            session_id,
            interval_ms,
            title,
        } => {
            publisher
                .publish_new_data_available(
                    &session_id,
                    interval_ms as u32,
                    title.as_deref(),
                )
                .await;
        }

        ChunkEvent::StreamDelta {
            session_id,
            lines,
            seq,
        } => {
            // ADR-035 M2: lines is Vec<(role, message_id, content)>.
            //
            // `seq` was assigned by `SessionCore::next_seq` at the emit
            // site and shipped through the chunk event unchanged. The
            // chunk_relay loop is single-threaded, so the seq here is
            // guaranteed to match the order in which this frame should
            // reach the Desktop — that is what makes the backend the
            // single source of truth for ordering, and lets the Desktop's
            // `insertBySeq` re-derive the right position even on rare
            // reorder.
            let lines_proto: Vec<StreamLine> = lines
                .into_iter()
                .map(|(role, message_id, content)| StreamLine {
                    role,
                    message_id,
                    line_no: 0,
                    content,
                })
                .collect();
            publisher
                .publish_stream_delta(&session_id, &lines_proto, seq)
                .await;
        }

        // ADR-035 C1: publish record_complete at QoS 1 (authoritative
        // terminal event). tool_result content is truncated to first 5
        // lines inside publish_record_complete (D9.2).
        //
        // `seq` travels with the ChunkEvent exactly as it was assigned at
        // the emit site (`SessionCore::next_seq`) — see the StreamDelta
        // branch above for the full ordering rationale.
        ChunkEvent::RecordComplete {
            session_id,
            role,
            message_id,
            content,
            tool_name,
            tool_call_id,
            is_error,
            seq,
        } => {
            publisher
                .publish_record_complete(
                    &session_id,
                    &role,
                    &message_id,
                    &content,
                    &tool_name,
                    &tool_call_id,
                    is_error,
                    seq,
                )
                .await;
        }

        ChunkEvent::AskQuestion {
            request_id,
            question,
            options,
            title,
            timeout_seconds,
        } => {
            let qjson = serde_json::json!({
                "request_id": request_id,
                "question": question,
                "options": options,
                "title": title,
                "timeout_seconds": timeout_seconds,
            });
            let question_json = qjson.to_string();
            publisher
                .publish_ask_question(sid, &request_id, &question_json)
                .await;
        }

        ChunkEvent::ClearRetainedEvent { event_type } => {
            publisher.clear_retained_event(sid, &event_type).await;
        }

        ChunkEvent::ToolProgress {
            session_id,
            tool_call_id,
            elapsed_ms,
            timeout_ms,
        } => {
            publisher
                .publish_tool_progress(
                    &session_id,
                    &tool_call_id,
                    elapsed_ms,
                    timeout_ms,
                )
                .await;
        }

        // ADR-XXX: per-session persisted metadata changed. Published
        // with Retained=true so a (re)connecting Desktop immediately
        // receives the current state via the broker's retained store.
        ChunkEvent::SessionConfigChanged { config } => {
            tracing::info!(
                sid = %sid,
                title = %config.title,
                model_id = %config.model_id,
                provider_id = %config.provider_id,
                workspace_id = %config.workspace_id,
                "Publishing session_config (retained)"
            );
            publisher.publish_session_config(sid, &config).await;
        }

        ChunkEvent::LoopDetectedPaused {
            message, ..
        } => {
            publisher.publish_loop_detected_paused(sid, &message).await;
        }
    }
}

/// Minimum interval between MQTT publishes for state changes (tokens,
/// Spawn the per-session config-change relay (ADR-043).
///
/// Config changes (title, model, provider, workspace_id, reasoning_effort,
/// temperature) are low-frequency user actions. The relay publishes
/// immediately with no throttle.
pub(crate) fn spawn_config_change_relay(
    mut config_rx: tokio::sync::mpsc::UnboundedReceiver<ConfigChange>,
    chunk_tx: tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>,
    conv: ConversationSession,
    session_id: String,
    // Late-bind slot for AgentCore. Used to run
    // resolve_effective_reasoning_effort before publishing so MQTT
    // subscribers see the same effective value as HTTP GET.
    // See agent::session_config::llm_effects for the rationale.
    core_slot: SharedAgentCore,
) {
    tokio::spawn(async move {
        use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};

        while let Some(change) = config_rx.recv().await {
            // Drop sentinel: empty session_id means the ConversationSession
            // is being dropped. Fall back to the relay's own clone.
            let mut config = if change.snapshot.session_id.is_empty()
                && change.snapshot.agent_id.is_empty()
            {
                conv.build_session_config_snapshot()
            } else {
                change.snapshot
            };

            // Resolve effective reasoning_effort before publish. The
            // raw value may be empty if the session was resumed from
            // a meta.json that never persisted reasoning_effort.
            // Funnel through the shared resolver so MQTT subscribers
            // see the same effective value as HTTP GET.
            if config.reasoning_effort.is_empty()
                && let Ok(slot) = core_slot.read()
                && let Some(core) = slot.as_ref()
            {
                let caps = core.get_model_capabilities(&config.model_id);
                if let Some(effort) =
                    crate::agent::session_config::llm_effects::resolve_effective_reasoning_effort(
                        caps.as_ref(),
                        None,
                    )
                {
                    config.reasoning_effort = effort.to_string();
                }
            }

            tracing::info!(
                session_id = %session_id,
                model_id = %config.model_id,
                provider_id = %config.provider_id,
                workspace_id = %config.workspace_id,
                "config relay: sending SessionConfigChanged to chunk channel"
            );

            let event = SessionChunkEvent {
                session_id: session_id.clone(),
                event: ChunkEvent::SessionConfigChanged { config },
            };
            if chunk_tx.try_send(event).is_err() {
                tracing::debug!(
                    session_id = %session_id,
                    "config relay: chunk channel full/closed, dropping notification"
                );
            }
        }
        tracing::debug!(
            session_id = %session_id,
            "config relay: config_rx closed, exiting"
        );
    });
}

/// Spawn the per-session state-change relay (ADR-043).
///
/// Every state change is published immediately — no cooldown. The retained
/// SessionState topic (`sessions/{sid}/state`) ensures the broker overwrites
/// the previous value, so even if a Desktop client is momentarily
/// disconnected, it receives the latest snapshot on reconnect.
pub(crate) fn spawn_state_change_relay(
    mut state_rx: tokio::sync::mpsc::UnboundedReceiver<StateChange>,
    chunk_tx: tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>,
    _conv: ConversationSession,
    session_id: String,
) {
    tokio::spawn(async move {
        use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};

        while let Some(change) = state_rx.recv().await {
            // Drop sentinel: empty session_id means the ConversationSession
            // is being dropped. Skip publishing — the last real state has
            // already been sent before the drop.
            if change.snapshot.session_id.is_empty()
                && change.snapshot.agent_id.is_empty()
            {
                continue;
            }

            let state = change.snapshot;
            tracing::info!(
                session_id = %session_id,
                message_count = state.message_count,
                "state relay: sending SessionStateChanged to chunk channel"
            );

            let event = SessionChunkEvent {
                session_id: session_id.clone(),
                event: ChunkEvent::SessionStateChanged { state },
            };
            if chunk_tx.try_send(event).is_err() {
                tracing::debug!(
                    session_id = %session_id,
                    "state relay: chunk channel full/closed, dropping notification"
                );
            }
        }
        tracing::debug!(
            session_id = %session_id,
            "state relay: state_rx closed, exiting"
        );
    });
}