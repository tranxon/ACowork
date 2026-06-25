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
    // Two channels are consumed:
    //   - chunk_rx: data events (Delta, ReasoningDelta, ToolCall, ...)
    //   - control_chunk_rx: control events (Stopped, SessionStateChanged,
    //     Done, Error, ...) — consumed with priority via biased select!
    // This separation ensures control events are never blocked by
    // data-event backpressure.
    let agent_id_for_relay = ctx.agent_id.clone();
    let chunk_relay = if ctx.chunk_rx.is_some() {
        let chunk_rx = ctx.chunk_rx.take().unwrap();
        let control_chunk_rx = ctx.control_chunk_rx.take();
        let outbound_tx = ctx
            .grpc_client
            .as_ref()
            .expect("grpc_client must be Some in Gateway mode")
            .outbound_sender();
        Some(tokio::spawn(async move {
            tracing::info!("Chunk relay started (shared gRPC connection)");
            let mut chunk_rx = chunk_rx;
            if let Some(mut control_rx) = control_chunk_rx {
                loop {
                    // Priority: drain control channel first (non-blocking)
                    while let Ok(session_event) = control_rx.try_recv() {
                        relay_chunk_event(
                            &outbound_tx,
                            &agent_id_for_relay,
                            &session_event.session_id,
                            session_event.event,
                        )
                        .await;
                    }
                    // Block-wait on both; biased gives control channel priority
                    tokio::select! {
                        biased;
                        session_event = control_rx.recv() => {
                            match session_event {
                                Some(ev) => {
                                    // Drain pending data events before sending
                                    // the control event. This preserves FIFO
                                    // ordering: all Deltas produced before the
                                    // Stopped/Done/Error are relayed first, then
                                    // the control event follows.
                                    //
                                    // try_recv is non-blocking — drains only
                                    // already-queued events without waiting.
                                    // This is safe because urgent_stop has
                                    // already halted the AgentLoop, so no new
                                    // Deltas will be produced during drain.
                                    while let Ok(data_event) = chunk_rx.try_recv() {
                                        relay_chunk_event(
                                            &outbound_tx,
                                            &agent_id_for_relay,
                                            &data_event.session_id,
                                            data_event.event,
                                        )
                                        .await;
                                    }
                                    // Now send the control event itself
                                    relay_chunk_event(
                                        &outbound_tx,
                                        &agent_id_for_relay,
                                        &ev.session_id,
                                        ev.event,
                                    )
                                    .await;
                                }
                                None => {
                                    // Control channel closed;
                                    // drain data channel then exit
                                    while let Some(ev) = chunk_rx.recv().await {
                                        relay_chunk_event(
                                            &outbound_tx,
                                            &agent_id_for_relay,
                                            &ev.session_id,
                                            ev.event,
                                        )
                                        .await;
                                    }
                                    break;
                                }
                            }
                        }
                        session_event = chunk_rx.recv() => {
                            match session_event {
                                Some(ev) => {
                                    relay_chunk_event(
                                        &outbound_tx,
                                        &agent_id_for_relay,
                                        &ev.session_id,
                                        ev.event,
                                    )
                                    .await;
                                }
                                None => {
                                    // Data channel closed;
                                    // drain control channel then exit
                                    while let Some(ev) = control_rx.recv().await {
                                        relay_chunk_event(
                                            &outbound_tx,
                                            &agent_id_for_relay,
                                            &ev.session_id,
                                            ev.event,
                                        )
                                        .await;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            } else {
                // No control channel (shouldn't happen in Gateway mode)
                while let Some(session_event) = chunk_rx.recv().await {
                    relay_chunk_event(
                        &outbound_tx,
                        &agent_id_for_relay,
                        &session_event.session_id,
                        session_event.event,
                    )
                    .await;
                }
            }
            tracing::debug!("Chunk relay task ended");
        }))
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

    // ── Send workspace config snapshot to Gateway ─────────────────────
    if let Some(ref mut client) = ctx.grpc_client {
        let config_path = work_dir_path
            .join("config")
            .join("agent_workspaces.json");
        let config_json = if config_path.exists() {
            std::fs::read_to_string(&config_path)
                .unwrap_or_else(|_| r#"{"version":"1.0.0","additional_dirs":[]}"#.to_string())
        } else {
            r#"{"version":"1.0.0","additional_dirs":[]}"#.to_string()
        };
        let msg = acowork_core::proto::ClientMessage {
            request_id: 0,
            payload: Some(
                acowork_core::proto::client_message::Payload::UpdateWorkspaceConfig(
                    acowork_core::proto::UpdateWorkspaceConfig { config_json },
                ),
            ),
        };
        if client.outbound_sender().send(msg).await.is_err() {
            tracing::warn!("Failed to send UpdateWorkspaceConfig snapshot to Gateway");
        } else {
            tracing::info!("Workspace config snapshot sent to Gateway");
        }
    }

    let (mcp_runtime_tx, mcp_runtime_rx) =
        tokio::sync::mpsc::channel::<crate::tools::mcp_manager::McpConnectResult>(1);

    Ok(SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    })
}

/// Dispatch a single `ChunkEvent` to the Gateway outbound channel.
///
/// Extracted from the chunk relay loop to keep the main spawn body readable.
async fn relay_chunk_event(
    outbound_tx: &tokio::sync::mpsc::Sender<acowork_core::proto::ClientMessage>,
    agent_id: &str,
    sid: &str,
    event: crate::agent::loop_::ChunkEvent,
) {
    use crate::agent::loop_::ChunkEvent;
    use crate::cli::{relay_intent, relay_stream_chunk, try_relay_stream_chunk};

    match event {
        ChunkEvent::ReasoningStarted => {
            let params = serde_json::json!({ "session_id": sid });
            relay_stream_chunk(outbound_tx, "agent_reasoning_started", &params).await;
        }

        ChunkEvent::Delta(delta) => {
            let params = serde_json::json!({ "content": delta, "session_id": sid });
            // Non-blocking: dropping a single delta is acceptable,
            // but blocking would stall control events in the relay loop.
            try_relay_stream_chunk(outbound_tx, "agent_chunk", &params);
        }

        ChunkEvent::ReasoningDelta(delta) => {
            let params = serde_json::json!({ "reasoning_content": delta, "session_id": sid });
            // Non-blocking: same rationale as Delta above.
            try_relay_stream_chunk(outbound_tx, "agent_chunk", &params);
        }

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
            if outbound_tx.send(msg).await.is_err() {
                tracing::debug!(
                    "Context usage report send failed — main connection may be closed"
                );
            }
        }

        ChunkEvent::CompactingStarted => {
            let params = serde_json::json!({ "session_id": sid });
            relay_intent(outbound_tx, "compacting_started", &params).await;
        }

        ChunkEvent::CompactingEnded => {
            let params = serde_json::json!({ "session_id": sid });
            relay_intent(outbound_tx, "compacting_ended", &params).await;
        }

        ChunkEvent::ToolCall { name, args, id } => {
            let parsed_args: serde_json::Value =
                serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({ "raw": args }));
            let params = serde_json::json!({
                "name": name, "params": parsed_args,
                "tool_call_id": id, "session_id": sid,
            });
            relay_intent(outbound_tx, "agent_tool_call", &params).await;
        }

        ChunkEvent::ToolResult { name, result, tool_call_id } => {
            let parsed_result: serde_json::Value = serde_json::from_str(&result)
                .unwrap_or_else(|_| serde_json::json!({ "content": result }));
            let params = serde_json::json!({
                "name": name, "result": parsed_result,
                "tool_call_id": tool_call_id, "session_id": sid,
            });
            relay_intent(outbound_tx, "agent_tool_result", &params).await;
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
            relay_intent(outbound_tx, "iteration_limit_paused", &params).await;
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
            relay_intent(outbound_tx, "tool_approval_needed", &params).await;
        }

        ChunkEvent::Done { content, message_id } => {
            let params = serde_json::json!({
                "content": content, "message_id": message_id, "session_id": sid,
            });
            relay_intent(outbound_tx, "agent_response", &params).await;
        }

        ChunkEvent::Error { user_message, detail, error_type, message_id } => {
            let params = serde_json::json!({
                "content": user_message,
                "detail": detail,
                "error_type": error_type,
                "message_id": message_id,
                "session_id": sid,
            });
            relay_intent(outbound_tx, "agent_error", &params).await;
        }

        ChunkEvent::Stopped { content } => {
            let params = serde_json::json!({ "content": content, "session_id": sid });
            relay_intent(outbound_tx, "agent_stopped", &params).await;
        }

        ChunkEvent::SessionStateChanged {
            status,
            model,
            provider,
            workspace_id,
            ratio,
            reasoning_effort,
            temperature,
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
            relay_intent(outbound_tx, "session_state_changed", &params).await;
        }

        ChunkEvent::TodoListUpdated { todos } => {
            let params = serde_json::json!({ "todos": todos, "session_id": sid });
            relay_intent(outbound_tx, "todo_list_updated", &params).await;
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
            relay_intent(outbound_tx, "ask_question", &params).await;
        }
    }
}
