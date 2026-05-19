"""Patch Gateway files for tool approval flow (C1+C2)"""

# ===== 1. grpc/dispatch.rs =====
path1 = r"d:\projects\rust\agent-study\core\rollball-gateway\src\grpc\dispatch.rs"
with open(path1, "r", encoding="utf-8") as f:
    content = f.read()

# Add import for ApprovalPendingRequests
old_import = "use crate::http::routes::SessionPendingRequests;"
new_import = "use crate::http::approval::ApprovalPendingRequests;\nuse crate::http::routes::SessionPendingRequests;"
content = content.replace(old_import, new_import)

# Add approval_pending parameter to dispatch_grpc_request
old_sig = """pub async fn dispatch_grpc_request(
    client_msg: proto::ClientMessage,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &Arc<Mutex<SessionManager>>,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: &Option<SessionPendingRequests>,
) -> proto::ServerMessage {"""
new_sig = """pub async fn dispatch_grpc_request(
    client_msg: proto::ClientMessage,
    conn_id: &str,
    state: &SharedState,
    session_mgr: &Arc<Mutex<SessionManager>>,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    session_pending: &Option<SessionPendingRequests>,
    approval_pending: &Option<ApprovalPendingRequests>,
) -> proto::ServerMessage {"""
if old_sig in content:
    content = content.replace(old_sig, new_sig)
    print("dispatch_grpc_request signature updated")
else:
    print("WARNING: dispatch_grpc_request signature NOT found!")

# Add tool_approval_needed handling BEFORE session_response check
old_intent = """        Some(proto::client_message::Payload::IntentSend(req)) => {
            let params: serde_json::Value = serde_json::from_str(&req.params_json)
                .unwrap_or(serde_json::Value::Null);

            // S1.14: Check if this is a session response from Runtime
            if req.action == "session_response" {"""
new_intent = """        Some(proto::client_message::Payload::IntentSend(req)) => {
            let params: serde_json::Value = serde_json::from_str(&req.params_json)
                .unwrap_or(serde_json::Value::Null);

            // C2: Intercept tool_approval_needed — create oneshot, send BridgeEvent,
            // await user decision from Desktop App via HTTP approval endpoint
            if req.action == "tool_approval_needed" && req.target == "http-api" {
                return handle_tool_approval_needed_grpc(
                    &params, &req.target, bridge_tx, approval_pending, request_id,
                ).await;
            }

            // S1.14: Check if this is a session response from Runtime
            if req.action == "session_response" {"""
if old_intent in content:
    content = content.replace(old_intent, new_intent)
    print("tool_approval_needed interception added")
else:
    print("WARNING: IntentSend handler NOT found!")

# Add the handler function before the existing handle_session_response_grpc
old_handler = """/// Handle session response from Runtime via gRPC (S1.14)."""
new_func = """/// Handle tool_approval_needed IntentSend from Runtime (C2).
///
/// Creates a oneshot channel, stores it in ApprovalPendingRequests,
/// sends a BridgeEvent::ToolApprovalNeeded to the Desktop App via WebSocket,
/// and awaits the user's decision (Allow/Deny) via the HTTP approval endpoint.
/// Returns a proto ServerMessage with an IntentDelivered that encodes the result.
async fn handle_tool_approval_needed_grpc(
    params: &serde_json::Value,
    target: &str,
    bridge_tx: &Option<tokio::sync::broadcast::Sender<BridgeEvent>>,
    approval_pending: &Option<ApprovalPendingRequests>,
    request_id: u64,
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
        request_id = %approval_request_id,
        agent_id = %agent_id,
        "Tool approval requested from Runtime"
    );

    // Step 1: Create oneshot channel and store in pending map
    let (tx, mut rx) = tokio::sync::oneshot::channel::<crate::http::approval::ApprovalResult>();

    if let Some(pending) = approval_pending {
        let mut map = pending.lock().await;
        map.insert(approval_request_id.to_string(), tx);
    } else {
        tracing::error!("No approval_pending map available — rejecting approval");
        return build_intent_delivered(request_id, &format!("denied:{}:no-approval-channel", approval_request_id));
    }

    // Step 2: Send BridgeEvent::ToolApprovalNeeded to Desktop App
    if let Some(tx) = bridge_tx {
        let event = crate::http::routes::BridgeEvent {
            agent_id: agent_id.to_string(),
            message_id: approval_request_id.to_string(),
            event_type: crate::http::routes::BridgeEventType::ToolApprovalNeeded,
            payload: params.clone(),
        };
        if let Err(e) = tx.send(event) {
            tracing::warn!(
                request_id = %approval_request_id,
                error = %e,
                "Failed to send ToolApprovalNeeded bridge event — no Desktop App subscribers"
            );
            // Clean up pending map
            if let Some(pending) = approval_pending {
                let mut map = pending.lock().await;
                map.remove(approval_request_id);
            }
            return build_intent_delivered(request_id, &format!("denied:{}:no-desktop-app", approval_request_id));
        }
    } else {
        tracing::warn!("No bridge channel for ToolApprovalNeeded event");
        return build_intent_delivered(request_id, &format!("denied:{}:no-bridge", approval_request_id));
    }

    // Step 3: Await user decision (with 60s timeout)
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        &mut rx,
    ).await;

    // Clean up any leftover pending entry
    if let Some(pending) = approval_pending {
        let mut map = pending.lock().await;
        map.remove(approval_request_id);
    }

    match result {
        Ok(Ok(approval_result)) => {
            tracing::info!(
                request_id = %approval_request_id,
                action = %approval_result.action,
                "Tool approval resolved"
            );
            match approval_result.action.as_str() {
                "allow" => build_intent_delivered(request_id, &format!("approved:{}", approval_request_id)),
                "allow_all_session" => build_intent_delivered(request_id, &format!("approved:{}", approval_request_id)),
                _ => build_intent_delivered(request_id, &format!("denied:{}:user-rejected", approval_request_id)),
            }
        }
        Ok(Err(_)) => {
            tracing::warn!(request_id = %approval_request_id, "Approval oneshot sender dropped");
            build_intent_delivered(request_id, &format!("denied:{}:channel-closed", approval_request_id))
        }
        Err(_) => {
            tracing::warn!(request_id = %approval_request_id, "Tool approval timed out after 60s");
            build_intent_delivered(request_id, &format!("denied:{}:timeout", approval_request_id))
        }
    }
}

fn build_intent_delivered(request_id: u64, message_id: &str) -> proto::ServerMessage {
    let response = crate::ipc::server::GatewayResponse::IntentDelivered {
        message_id: message_id.to_string(),
    };
    response.to_proto(request_id)
}

/// Handle session response from Runtime via gRPC (S1.14)."""

if old_handler in content:
    content = content.replace(old_handler, new_func)
    print("handle_tool_approval_needed_grpc added")
else:
    print("WARNING: handler comment NOT found!")

# Also update the call sites to pass approval_pending (2 call sites: stream chunk and normal dispatch)
old_stream_call = """                                    let _ = dispatch_grpc_request(
                                        client_msg,
                                        &conn_id_clone,
                                        &state,
                                        &ipc_session_mgr,
                                        &bridge_tx,
                                        &session_pending,
                                    ).await;"""
new_stream_call = """                                    let _ = dispatch_grpc_request(
                                        client_msg,
                                        &conn_id_clone,
                                        &state,
                                        &ipc_session_mgr,
                                        &bridge_tx,
                                        &session_pending,
                                        &approval_pending,
                                    ).await;"""
# This replacement is in grpc/server.rs, not dispatch.rs. Let me handle it there.

with open(path1, "w", encoding="utf-8") as f:
    f.write(content)
print("dispatch.rs updated")
