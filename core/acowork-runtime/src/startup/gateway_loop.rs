//! Phase D: announce ready and enter the main Gateway loop.
//!
//! Sends `AgentReady` to Gateway, then enters `run_gateway_loop`.
//! When the loop exits, waits for the chunk relay task to finish.
//!
//! ADR-033: Supports both gRPC and MQTT-only modes.

use crate::cli::LogReloadHandle;
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext};
use crate::startup::subsystems::SubsystemHandles;

/// Phase D: notify Gateway that the agent is ready, then run the message loop.
///
/// This is the last phase of the startup sequence.  It runs until the
/// Gateway connection is closed or a fatal error occurs.
pub(crate) async fn phase_d_run(
    ctx: &mut AgentBootContext,
    session_ctx: SessionBootContext,
    handles: SubsystemHandles,
    config: &RuntimeConfig,
    log_reload_handle: Option<LogReloadHandle>,
) -> Result<()> {
    let _span = tracing::info_span!("startup_phase_d").entered();

    let SessionBootContext {
        initial_session_id,
        mut session_manager,
        committed_lines: _committed_lines,
    } = session_ctx;

    let SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    } = handles;

    // ADR-033: MQTT control → session dispatch channel.
    let (mqtt_dispatch_tx, mqtt_dispatch_rx) = tokio::sync::mpsc::unbounded_channel();

    let _mqtt_handle = ctx.control_rx.take().map(|ctrl_rx| {
        let tx = mqtt_dispatch_tx.clone();
        tokio::spawn(async move {
            let mut rx = ctrl_rx;
            while let Some((topic, payload)) = rx.recv().await {
                let action = crate::mqtt::control_handler::parse_control_payload(&topic, &payload);
                match action {
                    Some(crate::mqtt::control_handler::ControlAction::SendMessage { session_id, message_id: _, content }) => {
                        let msg = crate::agent::inbound::InboundMessage::UserMessage(content);
                        let _ = tx.send((session_id, msg));
                    }
                    Some(crate::mqtt::control_handler::ControlAction::StopGeneration { session_id }) => {
                        let msg = crate::agent::inbound::InboundMessage::Stop { reason: "MQTT stop".to_string() };
                        let _ = tx.send((session_id, msg));
                    }
                    _ => {}
                }
            }
        })
    });

    if let Some(mut client) = ctx.grpc_client.take() {
        // ── gRPC Gateway mode ──────────────────────────────────────
        tracing::info!("All subsystems ready, sending AgentReady to Gateway via gRPC");
        {
            let agent_ready_msg = acowork_core::proto::ClientMessage {
                request_id: 0,
                payload: Some(acowork_core::proto::client_message::Payload::AgentReady(
                    acowork_core::proto::AgentReadyRequest {
                        agent_id: ctx.agent_id.clone(),
                    },
                )),
            };
            if client
                .outbound_ctrl_sender()
                .send(agent_ready_msg)
                .await
                .is_err()
            {
                tracing::warn!("Failed to send AgentReady to Gateway — stream may already be closed");
            } else {
                tracing::info!("AgentReady sent to Gateway for agent={}", ctx.agent_id);
            }
        }

        let gateway_query_rx = client.take_gateway_query_rx();

        let result = crate::cli::run_gateway_loop(
            &mut session_manager,
            &mut client,
            gateway_query_rx,
            config.work_dir.clone(),
            ctx.agent_id.clone(),
            ctx.version.clone(),
            log_reload_handle,
            ctx.skill_registry.clone(),
            ctx.workspace_resolver.clone(),
            initial_session_id,
            config.timeouts.session_idle_timeout_secs,
            config.max_sessions,
            config.timeouts.clone(),
            ctx.mcp_notifier.subscribe(),
            mcp_startup_rx,
            mcp_runtime_tx,
            mcp_runtime_rx,
            Some(mqtt_dispatch_rx),
        ).await;

        if let Some(handle) = chunk_relay {
            let _ = handle.await;
        }
        result
    } else if ctx.mqtt_client.is_some() {
        // ── ADR-033: MQTT-only Gateway mode ────────────────────────
        tracing::info!("All subsystems ready, announcing via MQTT (gRPC disabled)");
        if let Some(ref mqtt) = ctx.mqtt_client {
            let _ = mqtt.publish_status(true).await;
            tracing::info!("Agent status published via MQTT for agent={}", ctx.agent_id);
        }

        let result = mqtt_only_loop(
            &mut session_manager,
            mqtt_dispatch_rx,
            mcp_startup_rx,
            mcp_runtime_tx,
            mcp_runtime_rx,
            ctx.mcp_notifier.subscribe(),
            &config.work_dir,
        ).await;

        if let Some(handle) = chunk_relay {
            let _ = handle.await;
        }
        result
    } else {
        Err(crate::error::RuntimeError::Config(
            "Phase D entered without gRPC or MQTT client".into(),
        ))
    }
}

/// ADR-033: MQTT-only gateway loop — no gRPC dependency.
///
/// Listens for MQTT control dispatch and MCP events. The actual
/// chat loop runs in session tasks; this loop just routes messages.
async fn mqtt_only_loop(
    session_manager: &mut crate::agent::session::SessionManager,
    mut mqtt_dispatch_rx: tokio::sync::mpsc::UnboundedReceiver<(String, crate::agent::inbound::InboundMessage)>,
    mut mcp_startup_rx: Option<tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>>,
    mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_config_rx: tokio::sync::watch::Receiver<()>,
    work_dir: &str,
) -> Result<()> {
    tracing::info!("MQTT-only gateway loop started");
    let work_dir = work_dir.to_string();

    loop {
        tokio::select! {
            // MQTT control message dispatch
            dispatch_result = mqtt_dispatch_rx.recv() => {
                match dispatch_result {
                    Some((session_id, msg)) => {
                        if let Some(session) = session_manager.get_session(&session_id) {
                            let _ = session.send_inbound(msg);
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
                    session_manager.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                }
                mcp_startup_rx = None;
            }

            // Runtime MCP connect result
            mcp_runtime_result = mcp_runtime_rx.recv() => {
                if let Some((registry, wrappers, specs, failures)) = mcp_runtime_result {
                    session_manager.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                }
            }

            // MCP config change notification
            _ = mcp_config_rx.changed() => {
                tracing::info!("MCP config change — reconnecting MCP servers (background)");
                let merged = crate::agent_config::load_merged_mcp_configs(
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
                                prefixed_name.clone(), def, registry.clone(),
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
        }
    }

    tracing::info!("MQTT-only gateway loop ended");
    Ok(())
}
