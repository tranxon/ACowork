//! gRPC request dispatch — routes ClientMessage.payload to existing handler functions.
//!
//! This module converts proto request types into domain GatewayRequest variants,
//! delegates to the same handler functions used by the IPC server, and then
//! converts the domain GatewayResponse back into proto ServerMessage payloads.

use std::sync::Arc;
use tokio::sync::Mutex;

use rollball_core::proto;
use rollball_core::proto_bridge::GatewayResponseToProto;
use rollball_core::protocol::GatewayResponse;

use crate::http::routes::{BridgeEvent, SessionPendingRequests};
use crate::ipc::server::{
    handle_agent_hello, handle_budget_query, handle_capability_query,
    handle_context_usage_report, handle_cron_list, handle_cron_register,
    handle_cron_unregister, handle_intent_send,
    handle_key_release, handle_rate_acquire,
    handle_usage_report, handle_agent_ready, SharedState,
};
use crate::ipc::session::SessionManager;

/// Dispatch a proto ClientMessage to the appropriate handler and return a proto ServerMessage.
///
/// This function:
/// 1. Extracts the request_id from the ClientMessage
/// 2. Converts the proto payload into a domain GatewayRequest
/// 3. Calls the existing handler function
/// 4. Converts the domain GatewayResponse back into a proto ServerMessage
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_grpc_request(
    client_msg: proto::ClientMessage,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &Arc<Mutex<SessionManager>>,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: &Option<SessionPendingRequests>,
) -> proto::ServerMessage {
    let request_id = client_msg.request_id;

    let response = match client_msg.payload {
        Some(proto::client_message::Payload::KeyRelease(req)) => {
            handle_key_release(&req.provider, conn_id, state, session_mgr).await
        }

        Some(proto::client_message::Payload::IntentSend(req)) => {
            let params: serde_json::Value = serde_json::from_str(&req.params_json)
                .unwrap_or(serde_json::Value::Null);

            // C2: Intercept tool_approval_needed from Runtime.
            // Create oneshot, send BridgeEvent to Desktop App, await user decision.
            if req.action == "tool_approval_needed" && req.target == "http-api" {
                return handle_tool_approval_needed_grpc(
                    &params, bridge_tx,
                ).await;
            }

            // C3: Intercept ask_question from Runtime (ask_user_question tool).
            // Forward to Desktop App via BridgeEvent::AskQuestion — the user's
            // answer flows back via the HTTP question endpoint + gRPC push
            // (same unified push architecture as tool_approval_needed).
            if req.action == "ask_question" && req.target == "http-api" {
                return handle_ask_question_grpc(
                    &params, bridge_tx,
                ).await;
            }

            // S1.14: Check if this is a session response from Runtime
            if req.action == "session_response" {
                if let Some(pending) = session_pending {
                    handle_session_response_grpc(&params, pending).await;
                }
                GatewayResponse::IntentDelivered {
                    message_id: format!(
                        "msg-session-resp-{}",
                        chrono::Utc::now().timestamp_millis()
                    ),
                }
            } else {
                handle_intent_send(
                    &req.target,
                    &req.action,
                    &params,
                    req.r#async,
                    conn_id,
                    state,
                    session_mgr,
                    bridge_tx,
                )
                .await
            }
        }

        Some(proto::client_message::Payload::BudgetQuery(req)) => {
            handle_budget_query(&req.provider, state).await
        }

        Some(proto::client_message::Payload::UsageReport(req)) => {
            let report: rollball_core::budget::UsageReport = req.into();
            handle_usage_report(report, state).await
        }

        Some(proto::client_message::Payload::RateAcquire(req)) => {
            handle_rate_acquire(&req.provider, state).await
        }

        Some(proto::client_message::Payload::CapabilityQuery(req)) => {
            let agent_id = if req.agent_id.is_empty() {
                None
            } else {
                Some(req.agent_id.as_str())
            };
            handle_capability_query(agent_id, state).await
        }

        Some(proto::client_message::Payload::CronRegister(req)) => {
            let params: serde_json::Value = serde_json::from_str(&req.params_json)
                .unwrap_or(serde_json::Value::Null);
            handle_cron_register(&req.agent_id, &req.schedule, &req.action, &params, state).await
        }

        Some(proto::client_message::Payload::CronUnregister(req)) => {
            handle_cron_unregister(&req.cron_id, state).await
        }

        Some(proto::client_message::Payload::CronList(_req)) => {
            handle_cron_list(conn_id, session_mgr, state).await
        }

        Some(proto::client_message::Payload::ContextUsageReport(req)) => {
            let context: rollball_core::protocol::ContextUsageInfo = match req.context {
                Some(c) => c.into(),
                None => rollball_core::protocol::ContextUsageInfo {
                    context_window: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    max_input_tokens: None,
                    usable_context: 0,
                    usage_percent: 0,
                },
            };
            let agent_id = req.agent_id;
            handle_context_usage_report(&agent_id, &context, conn_id, session_mgr, bridge_tx).await
        }

        Some(proto::client_message::Payload::AgentHello(req)) => {
            handle_agent_hello(
                &req.agent_id,
                &req.version,
                &req.connection_role,
                req.provider_list_version,
                req.mcp_list_version,
                req.search_list_version,
                req.user_profile_version,
                conn_id,
                state,
                session_mgr,
            )
            .await
        }

        Some(proto::client_message::Payload::AgentReady(req)) => {
            handle_agent_ready(&req.agent_id, state).await
        }

        Some(proto::client_message::Payload::ListSessions(_req)) => {
            GatewayResponse::SessionList { sessions: vec![] }
        }

        Some(proto::client_message::Payload::GetSessionMessages(req)) => {
            tracing::warn!(
                session_id = %req.session_id,
                "GetSessionMessages via gRPC — session data is on the Runtime side, returning empty"
            );
            GatewayResponse::SessionMessages {
                messages: vec![],
                cursor: None,
                has_more: false,
            }
        }

        Some(proto::client_message::Payload::CreateSession(_req)) => {
            GatewayResponse::SessionCreated {
                session_id: String::new(),
            }
        }

        Some(proto::client_message::Payload::DeleteSession(_req)) => {
            GatewayResponse::SessionDeleted {
                success: false,
                error: Some("DeleteSession is handled by Runtime via IntentReceived".to_string()),
            }
        }

        Some(proto::client_message::Payload::GetCurrentSessionId(_req)) => {
            GatewayResponse::CurrentSessionId { session_id: None }
        }

        Some(proto::client_message::Payload::UpdateWorkspaceConfig(update)) => {
            // Runtime pushes its workspace config snapshot for Gateway's in-memory cache.
            let agent_id = {
                let mgr = session_mgr.lock().await;
                mgr.get_session(conn_id)
                    .and_then(|s| s.agent_id.clone())
            };
            if let Some(ref aid) = agent_id {
                let mut gw = state.write().await;
                if let Some(info) = gw.running_agents.get_mut(aid) {
                    info.workspace_config_json = Some(update.config_json);
                    tracing::info!(
                        agent_id = %aid,
                        "Updated RunningAgentInfo workspace config cache"
                    );
                }
            }
            return proto::ServerMessage {
                request_id,
                payload: None,
            };
        }

        Some(proto::client_message::Payload::ConfigSnapshot(snap)) => {
            // ConfigSnapshot is a response to QueryConfig.
            // The pending request mechanism will route this to the caller.
            // For now, the gRPC server handles ConfigSnapshot via the pending
            // request map in the session manager (same as Memory responses).
            tracing::debug!(
                request_id = snap.request_id,
                model = ?snap.model,
                provider = ?snap.provider,
                "Received ConfigSnapshot from Runtime"
            );
            return proto::ServerMessage {
                request_id,
                payload: None,
            };
        }

        Some(proto::client_message::Payload::StreamChunk(req)) => {
            // Stream chunks target the HTTP/WebSocket client — forward via bridge
            if req.target == "http-ws" || req.target == "http-api" {
                let agent_id = {
                    let mgr = session_mgr.lock().await;
                    mgr.get_session(conn_id)
                        .and_then(|s| s.agent_id.clone())
                        .unwrap_or_else(|| "unknown".to_string())
                };

                let params: serde_json::Value = serde_json::from_str(&req.params_json)
                    .unwrap_or(serde_json::Value::Null);

                let event_type = crate::http::routes::BridgeEventType::from_action(&req.action)
                    .unwrap_or_else(crate::http::routes::BridgeEventType::default_for_unknown);

                // Transparent passthrough: Gateway is a dumb pipe, not a protocol
                // translator. Only the Chunk event needs a minimal rename (content→delta)
                // to match the frontend's long-established streaming protocol.
                // All other events pass through Runtime's original params verbatim.
                let mut payload = params;
                if event_type == crate::http::routes::BridgeEventType::Chunk {
                    if let Some(content) = payload.get("content").and_then(|v| v.as_str()) {
                        payload["delta"] = serde_json::Value::String(content.to_string());
                    }
                }

                if let Some(tx) = bridge_tx {
                    let event = BridgeEvent {
                        agent_id,
                        message_id: format!("chunk-{}", chrono::Utc::now().timestamp_millis()),
                        event_type,
                        payload,
                    };
                    if let Err(e) = tx.send(event) {
                        tracing::debug!("Failed to broadcast stream chunk: {}", e);
                    }
                }
            }

            // Stream chunks produce no gRPC response
            return proto::ServerMessage {
                request_id,
                payload: None,
            };
        }

        None => {
            tracing::warn!(
                request_id,
                "ClientMessage with no payload — ignoring"
            );
            return proto::ServerMessage {
                request_id,
                payload: None,
            };
        }

        // Memory API response variants (MemoryNodesResult, MemoryStatsResult, etc.)
        // are handled by the gRPC session manager's pending request map,
        // not through the dispatch pathway.
        _ => {
            tracing::debug!(
                request_id,
                "ClientMessage payload not handled by dispatch (handled elsewhere)"
            );
            return proto::ServerMessage {
                request_id,
                payload: None,
            };
        }
    };

    // Convert domain GatewayResponse → proto ServerMessage
    response.to_proto(request_id)
}

/// Check if a ClientMessage is a stream chunk (no response expected).
pub fn is_stream_chunk(msg: &proto::ClientMessage) -> bool {
    matches!(
        msg.payload,
        Some(proto::client_message::Payload::StreamChunk(_))
    )
}

/// Handle tool_approval_needed IntentSend from Runtime (C2).
///
/// Creates a oneshot channel, stores it in ApprovalPendingRequests,
/// sends a BridgeEvent::ToolApprovalNeeded to the Desktop App via WebSocket,
/// Relay tool_approval_needed to Desktop App via BridgeEvent (pure relay).
/// No approval_pending map, no oneshot, no blocking — Runtime owns the approval state.
async fn handle_tool_approval_needed_grpc(
    params: &serde_json::Value,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
) -> proto::ServerMessage {
    let approval_request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(
        approval_req = %approval_request_id,
        agent_id = %agent_id,
        "Tool approval relayed from Runtime to Desktop App"
    );

    // Send BridgeEvent::ToolApprovalNeeded to Desktop App via WebSocket
    if let Some(tx_bridge) = bridge_tx {
        let event = BridgeEvent {
            agent_id: agent_id.to_string(),
            message_id: approval_request_id.to_string(),
            event_type: crate::http::routes::BridgeEventType::ToolApprovalNeeded,
            payload: params.clone(),
        };
        if let Err(e) = tx_bridge.send(event) {
            tracing::warn!(
                approval_req = %approval_request_id,
                error = %e,
                "Failed to send ToolApprovalNeeded bridge event — no Desktop App subscribers"
            );
        }
    } else {
        tracing::warn!("No bridge channel — cannot forward approval request");
    }

    // Return immediately — Runtime owns approval state, receives decision via
    // approval_decision IntentReceived pushed by HTTP handler.
    proto::ServerMessage { request_id: 0, payload: None }
}

/// Handle session response from Runtime via gRPC (S1.14).
///
/// Mirrors the IPC server's handle_session_response but accepts proto-compatible params.
async fn handle_session_response_grpc(
    params: &serde_json::Value,
    pending: &SessionPendingRequests,
) {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if request_id.is_empty() {
        tracing::warn!("Session response missing request_id, ignoring");
        return;
    }

    tracing::debug!(request_id = %request_id, "Received session response from Runtime (gRPC)");

    let mut map = pending.lock().await;
    if let Some(sender) = map.remove(request_id) {
        let response_value = params
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if sender.send(response_value).is_err() {
            tracing::warn!(
                request_id = %request_id,
                "Session response oneshot already closed — HTTP handler may have timed out"
            );
        }
    } else {
        tracing::warn!(
            request_id = %request_id,
            "Session response has no pending request — may have timed out"
        );
    }
}

/// Handle ask_question IntentSend from Runtime (C3).
///
/// Forwards the question to the Desktop App via BridgeEvent::AskQuestion
/// over WebSocket. The user's answer flows back via the HTTP question
/// endpoint, which pushes a `question_answer` IntentReceived back to
/// the Runtime (same unified push architecture as tool_approval_needed).
///
/// Unlike tool_approval_needed, there is no oneshot to await — the
/// answer is delivered to the Runtime via the push path only.
async fn handle_ask_question_grpc(
    params: &serde_json::Value,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
) -> proto::ServerMessage {
    let request_id = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(
        request_id = %request_id,
        agent_id = %agent_id,
        "Ask question requested from Runtime"
    );

    // Forward to Desktop App via WebSocket
    if let Some(tx_bridge) = bridge_tx {
        let event = BridgeEvent {
            agent_id: agent_id.to_string(),
            message_id: request_id.to_string(),
            event_type: crate::http::routes::BridgeEventType::AskQuestion,
            payload: params.clone(),
        };
        if let Err(e) = tx_bridge.send(event) {
            tracing::warn!(
                request_id = %request_id,
                error = %e,
                "Failed to send AskQuestion bridge event — no Desktop App subscribers"
            );
        }
    } else {
        tracing::warn!("No bridge channel — cannot forward ask_question event");
    }

    // Return immediately — the answer flows back via the push path:
    // HTTP question endpoint → IntentReceived push → Runtime InboundMessage::QuestionAnswer
    proto::ServerMessage { request_id: 0, payload: None }
}
