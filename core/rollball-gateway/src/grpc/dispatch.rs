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
    handle_cron_unregister, handle_identity_query, handle_intent_send,
    handle_key_release, handle_permission_request, handle_rate_acquire,
    handle_usage_report, SharedPermissionStore, SharedState,
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
    perm_store: &SharedPermissionStore,
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
                    perm_store,
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

        Some(proto::client_message::Payload::PermissionRequest(req)) => {
            handle_permission_request(
                &req.request_id,
                &req.permission,
                &req.reason,
                req.timeout_ms,
                conn_id,
                state,
                session_mgr,
                perm_store,
            )
            .await
        }

        Some(proto::client_message::Payload::IdentityQuery(req)) => {
            handle_identity_query(&req.fields, conn_id, session_mgr).await
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
                conn_id,
                state,
                session_mgr,
            )
            .await
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

                // Transform payload to match frontend WebSocket protocol
                let payload = match event_type {
                    crate::http::routes::BridgeEventType::Chunk => {
                        let delta = params.get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let mut p = serde_json::json!({ "delta": delta });
                        if let Some(reasoning) = params.get("reasoning_content").and_then(|v| v.as_str()) {
                            p["reasoning_content"] = serde_json::Value::String(reasoning.to_string());
                        }
                        p
                    }
                    _ => params,
                };

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
