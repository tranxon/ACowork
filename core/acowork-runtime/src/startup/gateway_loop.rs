//! Phase D: announce ready and enter the main Gateway loop.
//!
//! Sends `AgentReady` to Gateway, then enters `run_gateway_loop`.
//! When the loop exits, waits for the chunk relay task to finish.

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
    } = session_ctx;

    let SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    } = handles;

    // ctx.grpc_client must be Some in Gateway mode (Phase D is only called
    // when grpc_client is present).
    let mut client = ctx
        .grpc_client
        .take()
        .expect("grpc_client must be Some when entering Phase D");

    // ── Step 10: Notify Gateway that the agent is ready ──────────────
    tracing::info!("All subsystems ready, sending AgentReady to Gateway");
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
            .outbound_sender()
            .send(agent_ready_msg)
            .await
            .is_err()
        {
            tracing::warn!(
                "Failed to send AgentReady to Gateway — stream may already be closed"
            );
        } else {
            tracing::info!("AgentReady sent to Gateway for agent={}", ctx.agent_id);
        }
    }

    // Extract gateway query receiver before passing client to the loop.
    let gateway_query_rx = client.take_gateway_query_rx();

    let result = crate::cli::run_gateway_loop(
        &mut session_manager,
        &mut client,
        gateway_query_rx,
        config.work_dir.clone(),
        ctx.socket_path.clone(),
        ctx.agent_id.clone(),
        ctx.version.clone(),
        log_reload_handle,
        ctx.skill_registry.clone(),
        ctx.workspace_resolver.clone(),
        initial_session_id,
        config.session_idle_timeout_secs,
        ctx.mcp_notifier.subscribe(),
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    )
    .await;

    // Wait for chunk relay to drain and exit cleanly.
    if let Some(handle) = chunk_relay {
        let _ = handle.await;
    }

    result
}
