//! Phase C: spawn subsystems.
//!
//! Covers the latter part of Step 9:
//!   - Spawn chunk_relay task first so the chunk channel is draining
//!   - DevMode: start Debug Protocol server if --dev-mode
//!   - Sync agent_mcp.json catalog from Gateway hello
//!   - Spawn MCP auto-connect background task
//!   - Send workspace config snapshot to Gateway

use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext};

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
                    );
                }
                tracing::debug!("MQTT chunk relay task ended");
            }))
        } else if let Some(grpc_client) = ctx.grpc_client.as_ref() {
            // Legacy gRPC chunk relay.
            let chunk_rx = ctx.chunk_rx.take().unwrap();
            let outbound_ctrl_tx = grpc_client.outbound_ctrl_sender();
            Some(tokio::spawn(async move {
                tracing::info!("Chunk relay started (single channel)");
                let mut chunk_rx = chunk_rx;
                while let Some(session_event) = chunk_rx.recv().await {
                    relay_chunk_event(
                        &outbound_ctrl_tx,
                        &agent_id_for_relay,
                        &session_event.session_id,
                        session_event.event,
                    )
                    .await;
                }
                tracing::debug!("Chunk relay task ended");
            }))
        } else {
            None
        }
    } else {
        None
    };

    // ── DevMode: start Debug Protocol server ─────────────────────────
    if config.dev_mode {
        let debug_port = config.debug_port as u32;
        tracing::info!(
            debug_port = debug_port,
            "DevMode enabled at startup — starting Debug Protocol server"
        );
        session_ctx.session_manager.enable_debug_mode(debug_port).await;
    }

    // ── Sync agent_mcp.json catalog from Gateway hello ───────────────
    if let Some(ref cfg) = ctx.hello_config
        && let Some(ref mcp_list) = cfg.mcp_list
    {
        use acowork_core::protocol::McpServerConfigDef;
        let catalog: Vec<McpServerConfigDef> = mcp_list
            .iter()
            .map(|item| McpServerConfigDef {
                name: item.id.clone(),
                transport: item.transport.clone(),
                url: item.url.clone(),
                command: item.command.clone(),
                args: item.args.clone(),
                env: item.env.clone(),
                headers: item.headers.clone(),
                tool_timeout_secs: item.tool_timeout_secs,
            })
            .collect();
        if let Err(e) = crate::agent_config::save_agent_mcp_config_catalog(
            work_dir_path,
            &catalog,
        ) {
            tracing::warn!(
                error = %e,
                "Failed to sync agent_mcp.json catalog from AgentHello mcp_list"
            );
        } else {
            tracing::info!(
                catalog_count = catalog.len(),
                "Synced agent_mcp.json catalog from AgentHello mcp_list"
            );
        }
    }

    // ── MCP auto-connect at startup (background, non-blocking) ───────
    let mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    > = {
        let mcp_configs = crate::agent_config::load_merged_mcp_configs(work_dir_path);
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

/// Dispatch a single `ChunkEvent` to the Gateway outbound control channel.
///
/// ADR-021: All events go through the single outbound control channel.
/// Data events (Delta, ReasoningDelta, ReasoningStarted, ToolCall, ToolResult)
/// are no longer sent via channel — the frontend polls them via HTTP.
async fn relay_chunk_event(
    outbound_ctrl_tx: &tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    agent_id: &str,
    sid: &str,
    event: crate::agent::loop_::ChunkEvent,
) {
    use crate::agent::loop_::ChunkEvent;
    use crate::cli::relay_intent;

    match event {
        // ── Control events (blocking, must deliver) ─────────────────────

        ChunkEvent::ContextUsage(ctx_info) => {
            let msg = acowork_core::proto::ClientMessage {
                request_id: 0,
                payload: Some(
                    acowork_core::proto::client_message::Payload::ContextUsageReport(
                        acowork_core::proto::ContextUsageReportRequest {
                            agent_id: agent_id.to_string(),
                            context: Some((&ctx_info).into()),
                            session_id: sid.to_string(),
                        },
                    ),
                ),
            };
            if outbound_ctrl_tx.send(msg).await.is_err() {
                tracing::debug!(
                    "Context usage report send failed — main connection may be closed"
                );
            }
        }

        ChunkEvent::CompactingStarted => {
            let params = serde_json::json!({ "session_id": sid });
            relay_intent(outbound_ctrl_tx, "compacting_started", &params).await;
        }

        ChunkEvent::CompactingEnded => {
            let params = serde_json::json!({ "session_id": sid });
            relay_intent(outbound_ctrl_tx, "compacting_ended", &params).await;
        }

        ChunkEvent::IterationLimitPaused { iteration, max_iterations } => {
            let params = serde_json::json!({
                "iteration": iteration,
                "max_iterations": max_iterations,
                "message": format!(
                    "Iteration limit reached ({}/{}). Click Continue to keep going.",
                    iteration, max_iterations
                ),
                "session_id": sid,
            });
            relay_intent(outbound_ctrl_tx, "iteration_limit_paused", &params).await;
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
            let params = serde_json::json!({
                "request_id": request_id,
                "agent_id": agent_id,
                "tool_name": tool_name,
                "action": action,
                "risk_level": risk_level,
                "reason": reason,
                "session_id": sid,
                "tool_call_id": tool_call_id,
                "approval_timeout_secs": approval_timeout_secs,
            });
            relay_intent(outbound_ctrl_tx, "tool_approval_needed", &params).await;
        }

        ChunkEvent::Done { content, message_id } => {
            let params = serde_json::json!({
                "content": content, "message_id": message_id, "session_id": sid,
            });
            relay_intent(outbound_ctrl_tx, "agent_response", &params).await;
        }

        ChunkEvent::Error { user_message, detail, error_type, message_id } => {
            let params = serde_json::json!({
                "content": user_message,
                "detail": detail,
                "error_type": error_type,
                "message_id": message_id,
                "session_id": sid,
            });
            relay_intent(outbound_ctrl_tx, "agent_error", &params).await;
        }

        ChunkEvent::Stopped { content } => {
            let params = serde_json::json!({ "content": content, "session_id": sid });
            relay_intent(outbound_ctrl_tx, "agent_stopped", &params).await;
        }

        ChunkEvent::SessionStateChanged {
            status,
            model,
            provider,
            workspace_id,
            ratio,
            reasoning_effort,
            temperature,
            context_usage,
        } => {
            let mut params = serde_json::json!({ "status": status, "session_id": sid });
            if let Some(ref m) = model {
                params["model"] = serde_json::json!(m);
            }
            if let Some(ref p) = provider {
                params["provider"] = serde_json::json!(p);
            }
            if let Some(ref w) = workspace_id {
                params["workspace_id"] = serde_json::json!(w);
            }
            if let Some(r) = ratio {
                params["ratio"] = serde_json::json!(r);
            }
            if let Some(ref re) = reasoning_effort {
                params["reasoning_effort"] = serde_json::json!(re);
            }
            if let Some(t) = temperature {
                params["temperature"] = serde_json::json!(t);
            }
            if let Some(ref cu) = context_usage
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(cu)
            {
                params["context_usage"] = v;
            }
            relay_intent(outbound_ctrl_tx, "session_state_changed", &params).await;
        }

        ChunkEvent::TodoListUpdated { todos } => {
            let params = serde_json::json!({ "todos": todos, "session_id": sid });
            relay_intent(outbound_ctrl_tx, "todo_list_updated", &params).await;
        }

        ChunkEvent::NewDataAvailable {
            session_id,
            interval_ms,
            title,
        } => {
            let params = serde_json::json!({
                "session_id": session_id,
                "interval_ms": interval_ms,
                "title": title,
            });
            relay_intent(outbound_ctrl_tx, "new_data_available", &params).await;
        }

        ChunkEvent::AskQuestion {
            request_id,
            question,
            options,
            title,
            timeout_seconds,
        } => {
            let params = serde_json::json!({
                "request_id": request_id,
                "question": question,
                "options": options,
                "title": title,
                "timeout_seconds": timeout_seconds,
                "agent_id": agent_id,
                "session_id": sid,
            });
            relay_intent(outbound_ctrl_tx, "ask_question", &params).await;
        }
    }
}

// ── ADR-033: MQTT chunk relay ─────────────────────────────────────────

/// Relay a `ChunkEvent` to the MQTT broker via `MqttChunkPublisher`.
///
/// All events are encoded as `DataEnvelope { payload: SessionMessage }`
/// protobuf and published to `acowork/agents/{id}/sessions/{sid}/messages/{event_type}`.
/// Desktop subscribes to these topics directly on the broker.
fn relay_chunk_event_mqtt(
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
            publisher.publish_context_usage(sid, &ctx_info);
        }

        ChunkEvent::CompactingStarted => {
            publisher.publish_compacting(sid, true);
        }

        ChunkEvent::CompactingEnded => {
            publisher.publish_compacting(sid, false);
        }

        ChunkEvent::IterationLimitPaused {
            iteration,
            max_iterations,
        } => {
            publisher.publish_iteration_limit_paused(sid, iteration, max_iterations);
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
            publisher.publish_tool_approval_needed(
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
            );
        }

        ChunkEvent::Done {
            message_id, ..
        } => {
            publisher.publish_done(sid, &message_id);
        }

        ChunkEvent::Error {
            user_message,
            detail: _,
            error_type: _,
            message_id,
        } => {
            publisher.publish_error(sid, &message_id, &user_message);
        }

        ChunkEvent::Stopped { .. } => {
            // Use empty message_id for stopped — the event itself is the signal.
            publisher.publish_stopped(sid, "");
        }

        ChunkEvent::SessionStateChanged {
            status,
            model,
            provider,
            workspace_id,
            ratio,
            reasoning_effort,
            temperature,
            context_usage,
        } => {
            let status_json = serde_json::to_string(&status).unwrap_or_default();
            publisher.publish_session_state_changed(
                crate::mqtt::client::SessionStateChangeEvent {
                    session_id: sid,
                    status_json: &status_json,
                    model: model.as_deref(),
                    provider: provider.as_deref(),
                    workspace_id: workspace_id.as_deref(),
                    ratio,
                    reasoning_effort: reasoning_effort.as_deref(),
                    temperature,
                    context_usage_json: context_usage.as_deref(),
                },
            );
        }

        ChunkEvent::TodoListUpdated { todos } => {
            let todos_json = serde_json::to_string(&todos).unwrap_or_default();
            publisher.publish_todo_updated(sid, &todos_json);
        }

        ChunkEvent::NewDataAvailable {
            session_id,
            interval_ms,
            title,
        } => {
            publisher.publish_new_data_available(
                &session_id,
                interval_ms as u32,
                title.as_deref(),
            );
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
            publisher.publish_ask_question(sid, &request_id, &question_json);
        }
    }
}
