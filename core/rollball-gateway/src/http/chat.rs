//! Chat/conversation HTTP API handlers
//!
//! Implements the conversation endpoints:
//! - POST /api/agents/:id/message — send a message (fire-and-forget)
//! - GET  /api/agents/:id/stream  — WebSocket upgrade for streaming
//!
//! WebSocket message format:
//!   Client → Server:  { "type": "message", "content": "..." }
//!   Server → Client:  { "type": "chunk", "delta": "...", "message_id": "..." }
//!                     { "type": "tool_call", "name": "...", "params": {...} }
//!                     { "type": "tool_result", "name": "...", "result": {...} }
//!                     { "type": "done", "message_id": "...", "usage": {...} }

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Router,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Maximum content length for a single message (32 KB)
const MAX_CONTENT_LENGTH: usize = 32 * 1024;

/// Build the chat/conversation router
pub fn chat_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/message", post(send_message))
        .route("/api/agents/{id}/stream", get(agent_stream_ws))
        .route("/api/agents/{id}/conversations", get(get_conversations))
        .route("/api/agents/{id}/conversations/latest", get(get_latest_conversation))
        .route("/api/agents/{id}/sessions", get(list_sessions).post(create_session))
        .route("/api/agents/{id}/sessions/{session_id}/activate", post(activate_session))
        .route("/api/agents/{id}/sessions/{session_id}/title", put(update_session_title))
        .route("/api/agents/{id}/sessions/{session_id}/messages", get(get_session_messages))
        .route("/api/agents/{id}/sessions/{session_id}", delete(delete_session))
        .route("/api/agents/{id}/continue", post(continue_execution))
}

// ── Request/Response types ────────────────────────────────────────────

/// Request body for sending a message
#[derive(Deserialize)]
pub struct SendMessageRequest {
    /// The message content
    pub content: String,
    /// Optional conversation ID for multi-turn
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Skill command selected by the user (e.g. "/commit", "/review-pr")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Response for send message
#[derive(Serialize)]
pub struct SendMessageResponse {
    /// Unique message ID for correlation
    pub message_id: String,
    /// Delivery status
    pub status: String,
}

/// A single conversation session summary
#[derive(Serialize)]
pub struct ConversationSummary {
    /// Session identifier
    pub session_id: String,
    /// Unix timestamp (seconds) when the session started
    pub started_at: i64,
    /// Number of messages in the session
    pub message_count: u32,
    /// Unix timestamp (seconds) of the most recent message
    pub last_message_at: i64,
}

/// Response for listing conversation sessions
#[derive(Serialize)]
pub struct ConversationsListResponse {
    /// List of conversation sessions
    pub conversations: Vec<ConversationSummary>,
}

/// A single message within a conversation
#[derive(Serialize)]
pub struct ConversationMessage {
    /// Role: "user" | "assistant" | "tool"
    pub role: String,
    /// Message content
    pub content: String,
    /// Unix timestamp (seconds)
    pub timestamp: i64,
    /// Turn index within the session
    pub turn_index: u32,
}

/// Response for the latest conversation
#[derive(Serialize)]
pub struct LatestConversationResponse {
    /// Session identifier
    pub session_id: String,
    /// Messages in the conversation, sorted by turn_index
    pub messages: Vec<ConversationMessage>,
}

/// WebSocket client message (inbound from Desktop App)
#[derive(Deserialize)]
struct WsClientMessage {
    #[serde(rename = "type")]
    msg_type: String,
    content: Option<String>,
    /// Model name for model_switch messages
    model: Option<String>,
    /// Provider name for model_switch messages
    provider: Option<String>,
    /// Skill command selected by the user (e.g. "/commit", "/review-pr")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `POST /api/agents/:id/message` — send a message to an agent
///
/// Validates the agent exists and is running, then pushes the message
/// to the agent's IPC session via the SessionManager.
/// Returns a message_id for correlation.
pub async fn send_message(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<SendMessageResponse>), (StatusCode, Json<ApiError>)> {
    // Validate agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // P1-2 fix: Validate conversation_id format
    if let Some(conv_id) = &body.conversation_id {
        if conv_id.len() > 128 {
            return Err(ApiError::bad_request("conversation_id too long (max 128 characters)"));
        }
        if !conv_id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return Err(ApiError::bad_request("conversation_id contains invalid characters (only alphanumeric, '-', '_' allowed)"));
        }
    }

    // Validate content length
    if body.content.is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if body.content.len() > MAX_CONTENT_LENGTH {
        return Err(ApiError::bad_request(&format!(
            "content too long (max {} bytes, got {})",
            MAX_CONTENT_LENGTH,
            body.content.len()
        )));
    }

    // Generate message ID
    let message_id = format!("msg-{}", uuid::Uuid::new_v4());

    // Push message to agent via SessionManager (if available)
    // S1.6 will implement the full response bridge
    if let Some(session_mgr) = &state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                from: "http-api".to_string(),
                action: "chat_message".to_string(),
                params: serde_json::json!({
                    "content": body.content,
                    "message_id": message_id,
                    "conversation_id": body.conversation_id,
                }),
                command: body.command.clone(),
            };
            let pushed = session.push_message(intent).await;
            if !pushed {
                tracing::warn!(
                    "Failed to push message to agent {} via conn {}",
                    agent_id,
                    conn_id
                );
                return Err(ApiError::internal("Failed to deliver message to agent"));
            }
        } else {
            tracing::warn!("Agent {} is running but has no IPC session", agent_id);
            return Err(ApiError::service_unavailable(&format!(
                "Agent {} is not yet connected",
                agent_id
            )));
        }
    }

    Ok((
        StatusCode::OK,
        Json(SendMessageResponse {
            message_id,
            status: "sent".to_string(),
        }),
    ))
}

/// `GET /api/agents/:id/conversations` — list conversation sessions for an agent
///
/// S1.14: Forwards the query to Runtime via IPC (IntentReceived push)
/// and waits for the response. Falls back to the legacy Grafeo-based
/// implementation if the agent is not running via IPC.
pub async fn get_conversations(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<ConversationsListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // S1.14: Try IPC forwarding first (if agent is running)
    if let Some(ref session_mgr) = state.session_mgr {
        let request_id = format!("sess-list-{}", uuid::Uuid::new_v4());
        let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "list_sessions".to_string(),
            params: serde_json::json!({
                "request_id": request_id,
            }),
            command: None,
        };

        let pushed = {
            let mgr = session_mgr.lock().await;
            if let Some((_, session)) = mgr.find_by_agent_id(&agent_id) {
                session.push_message(intent).await
            } else {
                false
            }
        }; // mgr dropped here

        if pushed {
            // Wait for Runtime response via IPC
            match wait_for_session_response(&state, &request_id).await {
                Ok(data) => {
                    // Convert SessionInfoDto to ConversationSummary
                    let sessions: Vec<rollball_core::protocol::SessionInfoDto> =
                        data.get("sessions")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                    let conversations: Vec<ConversationSummary> = sessions
                        .into_iter()
                        .map(|s| ConversationSummary {
                            session_id: s.session_id,
                            started_at: parse_iso8601_to_unix(&s.created_at),
                            message_count: s.message_count,
                            last_message_at: parse_iso8601_to_unix(&s.created_at),
                        })
                        .collect();
                    return Ok(Json(ConversationsListResponse { conversations }));
                }
                Err(e) => {
                    tracing::warn!("Session IPC query timed out or failed: {}, falling back to Grafeo", e);
                }
            }
        }
    }

    // Legacy fallback: use Grafeo memory store
    let conversations = get_conversations_legacy(&state, &agent_id).await;
    Ok(Json(ConversationsListResponse { conversations }))
}

/// `GET /api/agents/:id/conversations/latest` — get the most recent conversation
///
/// S1.14: Forwards the query to Runtime via IPC. Falls back to the
/// legacy Grafeo-based implementation if IPC is unavailable.
pub async fn get_latest_conversation(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<LatestConversationResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // S1.14: Try IPC forwarding first (if agent is running)
    if let Some(ref session_mgr) = state.session_mgr {
        // First, get current session ID
        let curr_request_id = format!("sess-curr-{}", uuid::Uuid::new_v4());
        let curr_intent = rollball_core::protocol::GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "get_current_session_id".to_string(),
            params: serde_json::json!({
                "request_id": curr_request_id,
            }),
            command: None,
        };

        let curr_pushed = {
            let mgr = session_mgr.lock().await;
            if let Some((_, session)) = mgr.find_by_agent_id(&agent_id) {
                session.push_message(curr_intent).await
            } else {
                false
            }
        }; // mgr dropped here

        if curr_pushed
            && let Ok(data) = wait_for_session_response(&state, &curr_request_id).await
        {
            let current_session_id = data.get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !current_session_id.is_empty() {
                // Now get the messages for this session
                let msg_request_id = format!("sess-msgs-{}", uuid::Uuid::new_v4());
                let msg_intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                    from: "http-api".to_string(),
                    action: "get_session_messages".to_string(),
                    params: serde_json::json!({
                        "request_id": msg_request_id,
                        "session_id": current_session_id,
                        "limit": 100,
                        "direction": "backward",
                    }),
                    command: None,
                };

                let msg_pushed = {
                    let mgr = session_mgr.lock().await;
                    if let Some((_, session)) = mgr.find_by_agent_id(&agent_id) {
                        session.push_message(msg_intent).await
                    } else {
                        false
                    }
                }; // mgr dropped here

                if msg_pushed
                    && let Ok(msg_data) = wait_for_session_response(&state, &msg_request_id).await
                {
                    let messages: Vec<ConversationMessage> = msg_data.get("messages")
                        .and_then(|v| serde_json::from_value::<Vec<rollball_core::protocol::ConversationEntryDto>>(v.clone()).ok())
                        .unwrap_or_default()
                        .into_iter()
                        .enumerate()
                        .map(|(i, m)| ConversationMessage {
                            role: m.role,
                            content: m.content,
                            timestamp: parse_iso8601_to_unix(&m.ts),
                            turn_index: i as u32,
                        })
                        .collect();

                    return Ok(Json(LatestConversationResponse {
                        session_id: current_session_id,
                        messages,
                    }));
                }
            }
        }
    }

    // Legacy fallback: use Grafeo memory store
    let (session_id, messages) = get_latest_conversation_legacy(&state, &agent_id).await;
    Ok(Json(LatestConversationResponse {
        session_id,
        messages,
    }))
}

/// `GET /api/agents/:id/stream` — WebSocket upgrade for streaming chat
///
/// Upgrades the HTTP connection to a WebSocket for bidirectional
/// streaming communication with an agent.
pub async fn agent_stream_ws(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    // Validate agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
    }

    // Upgrade to WebSocket
    Ok(ws.on_upgrade(move |socket| handle_ws(socket, agent_id, state)))
}

/// Handle the WebSocket connection lifecycle
///
/// Receives messages from the client, pushes them to the Agent's IPC session,
/// and subscribes to the bridge channel for streaming responses back.
async fn handle_ws(mut socket: WebSocket, agent_id: String, state: AppState) {
    tracing::info!("WebSocket connected for agent: {}", agent_id);

    // Subscribe to bridge channel for this agent's responses
    let mut bridge_rx = state.bridge_tx.as_ref().map(|tx| tx.subscribe());

    // Send initial connection acknowledgment
    let welcome = serde_json::json!({
        "type": "connected",
        "agent_id": agent_id,
    });
    let _ = socket.send(Message::Text(welcome.to_string().into())).await;

    loop {
        tokio::select! {
            // Branch 1: Incoming message from client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_ws_text(&mut socket, &agent_id, &state, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("WebSocket closed for agent: {}", agent_id);
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    _ => {
                        // Ignore binary, pong, etc.
                    }
                }
            }
            // Branch 2: Bridge event from Agent (streaming response)
            bridge_event = async {
                match &mut bridge_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match bridge_event {
                    Ok(event) => {
                        // Only forward events for this agent
                        if event.agent_id == agent_id {
                            // Build the WebSocket message matching frontend protocol:
                            //   { "type": "chunk", "delta": "...", "message_id": "..." }
                            //   { "type": "done", "content": "...", "message_id": "..." }
                            //   { "type": "error", "message": "...", "message_id": "..." }
                            let mut json = serde_json::json!({
                                "type": event.event_type.as_str(),
                                "message_id": event.message_id,
                            });
                            // Merge payload fields into the top-level JSON
                            if let serde_json::Value::Object(map) = event.payload {
                                for (k, v) in map {
                                    json[&k] = v;
                                }
                            }
                            let _ = socket.send(Message::Text(json.to_string().into())).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Bridge channel lagged for {}: skipped {} events", agent_id, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("Bridge channel closed for agent: {}", agent_id);
                        break;
                    }
                }
            }
        }
    }
}

/// Handle a single text message from the WebSocket client
async fn handle_ws_text(
    socket: &mut WebSocket,
    agent_id: &str,
    state: &AppState,
    text: &str,
) {
    // Parse client message
    let client_msg: WsClientMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            let err = serde_json::json!({
                "type": "error",
                "message": format!("Invalid message format: {}", e),
            });
            let _ = socket.send(Message::Text(err.to_string().into())).await;
            return;
        }
    };

    if client_msg.msg_type == "model_switch" {
        // Handle model switch: push to running agent via IPC
        // Only running agents can switch models — persistence is handled by Agent Runtime
        let model = match client_msg.model {
            Some(ref m) if !m.is_empty() => m.clone(),
            _ => {
                let err = serde_json::json!({
                    "type": "error",
                    "message": "model_switch requires a non-empty model field",
                });
                let _ = socket.send(Message::Text(err.to_string().into())).await;
                return;
            }
        };

        let provider = client_msg.provider.filter(|p| !p.is_empty());
        tracing::info!(agent = %agent_id, model = %model, provider = ?provider, "Forwarding model_switch to agent");

        let message_id = format!("msg-{}", uuid::Uuid::new_v4());
        let mut pushed_ok = false;
        if let Some(session_mgr) = &state.session_mgr {
            let mgr = session_mgr.lock().await;
            if let Some((_, session)) = mgr.find_by_agent_id(agent_id) {
                let mut params = serde_json::json!({
                    "model": model,
                    "message_id": message_id,
                });
                if let Some(ref p) = provider {
                    params["provider"] = serde_json::json!(p);
                }
                let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                    from: "http-ws".to_string(),
                    action: "model_switch".to_string(),
                    params,
                    command: None,
                };
                pushed_ok = session.push_message(intent).await;
            }
        }

        if pushed_ok {
            // Push LLMConfigDelivery so that Runtime rebuilds the Provider instance
            // when the provider changes (e.g. deepseek → minimax-cn which needs Anthropic protocol).
            // Always push when the provider field is present — Runtime decides whether
            // to rebuild based on protocol_type change detection.
            if let Some(ref provider_name) = provider {
                push_llm_config_on_switch(
                    state, agent_id, provider_name, &model,
                ).await;
            }

            let ack = serde_json::json!({
                "type": "ack",
                "message_id": message_id,
            });
            let _ = socket.send(Message::Text(ack.to_string().into())).await;
            let mut confirmed = serde_json::json!({
                "type": "model_confirmed",
                "model": model,
                "agentId": agent_id,
            });
            if let Some(ref p) = provider {
                confirmed["provider"] = serde_json::json!(p);
            }
            let _ = socket.send(Message::Text(confirmed.to_string().into())).await;
        } else {
            let err = serde_json::json!({
                "type": "error",
                "message": format!("Agent {} is not running, cannot switch model", agent_id),
                "message_id": message_id,
                "agentId": agent_id,
            });
            let _ = socket.send(Message::Text(err.to_string().into())).await;
        }
        return;
    }

    if client_msg.msg_type == "stop" {
        // Handle stop: send interrupt signal to running agent via IPC
        tracing::info!(agent = %agent_id, "Forwarding stop signal to agent");

        let mut pushed_ok = false;
        if let Some(session_mgr) = &state.session_mgr {
            let mgr = session_mgr.lock().await;
            if let Some((_, session)) = mgr.find_by_agent_id(agent_id) {
                let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                    from: "http-ws".to_string(),
                    action: "interrupt".to_string(),
                    params: serde_json::json!({
                        "reason": "user_requested",
                    }),
                    command: None,
                };
                pushed_ok = session.push_message(intent).await;
            }
        }

        if pushed_ok {
            let ack = serde_json::json!({
                "type": "stopped",
                "agentId": agent_id,
            });
            let _ = socket.send(Message::Text(ack.to_string().into())).await;
        } else {
            let err = serde_json::json!({
                "type": "error",
                "message": format!("Agent {} is not running, cannot stop", agent_id),
                "agentId": agent_id,
            });
            let _ = socket.send(Message::Text(err.to_string().into())).await;
        }
        return;
    }

    if client_msg.msg_type != "message" {
        let err = serde_json::json!({
            "type": "error",
            "message": format!("Unknown message type: {}", client_msg.msg_type),
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    }

    let content = client_msg.content.unwrap_or_default();

    // Validate content length for WebSocket messages too
    if content.is_empty() {
        let err = serde_json::json!({
            "type": "error",
            "message": "content must not be empty",
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    }
    if content.len() > MAX_CONTENT_LENGTH {
        let err = serde_json::json!({
            "type": "error",
            "message": format!("content too long (max {} bytes)", MAX_CONTENT_LENGTH),
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    }

    let message_id = format!("msg-{}", uuid::Uuid::new_v4());

    // Push to agent via SessionManager
    let mut pushed_ok = false;
    if let Some(session_mgr) = &state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_, session)) = mgr.find_by_agent_id(agent_id) {
            let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                from: "http-ws".to_string(),
                action: "chat_message".to_string(),
                params: serde_json::json!({
                    "content": content,
                    "message_id": message_id,
                }),
                command: client_msg.command.clone(),
            };
            pushed_ok = session.push_message(intent).await;
        }
    }

    if !pushed_ok {
        let err = serde_json::json!({
            "type": "error",
            "message": format!("Agent {} is not connected via IPC", agent_id),
            "message_id": message_id,
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    }

    // Acknowledge message received — the actual Agent response
    // (chunk/tool_call/tool_result/done) will arrive via bridge_rx.
    let ack = serde_json::json!({
        "type": "ack",
        "message_id": message_id,
    });
    let _ = socket.send(Message::Text(ack.to_string().into())).await;
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_message_request_deserialization() {
        let json = r#"{"content": "Hello, agent!"}"#;
        let req: SendMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Hello, agent!");
        assert!(req.conversation_id.is_none());
        assert!(req.command.is_none());
    }

    #[test]
    fn test_send_message_request_with_conversation_id() {
        let json = r#"{"content": "Hello!", "conversation_id": "conv-123"}"#;
        let req: SendMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Hello!");
        assert_eq!(req.conversation_id, Some("conv-123".to_string()));
        assert!(req.command.is_none());
    }

    #[test]
    fn test_send_message_request_with_command() {
        let json = r#"{"content": "Fix the bug", "command": "/commit"}"#;
        let req: SendMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Fix the bug");
        assert_eq!(req.command, Some("/commit".to_string()));
    }

    #[test]
    fn test_send_message_response_serialization() {
        let resp = SendMessageResponse {
            message_id: "msg-abc".to_string(),
            status: "sent".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("msg-abc"));
        assert!(json.contains("sent"));
    }

    #[test]
    fn test_ws_client_message_deserialization() {
        let json = r#"{"type": "message", "content": "Hi there"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "message");
        assert_eq!(msg.content, Some("Hi there".to_string()));
        assert!(msg.command.is_none());
    }
    
    #[test]
    fn test_ws_client_message_with_command() {
        let json = r#"{"type": "message", "content": "Review code", "command": "/review-pr"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "message");
        assert_eq!(msg.content, Some("Review code".to_string()));
        assert_eq!(msg.command, Some("/review-pr".to_string()));
    }

    #[test]
    fn test_content_length_limit() {
        // 32KB is the limit
        assert_eq!(MAX_CONTENT_LENGTH, 32 * 1024);
    }

    #[test]
    fn test_conversation_id_valid_format() {
        // Valid formats
        let valid_ids = ["conv-123", "abc_def", "ABC123", "conv-2024-01-01"];
        for id in &valid_ids {
            assert!(id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
        }
    }

    #[test]
    fn test_conversation_id_invalid_chars() {
        // Invalid: contains spaces, dots, slashes
        let invalid_ids = ["conv 123", "conv.123", "conv/123", "conv@123"];
        for id in &invalid_ids {
            assert!(!id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
        }
    }
}

// ── Continue Execution API ────────────────────────────────────────────

/// Continue agent execution after iteration limit was reached.
///
/// This sends a `ContinueExecution` signal to the Agent Runtime via IPC,
/// which resets the iteration counter and resumes the agent loop.
pub async fn continue_execution(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiError>)> {
    // Validate agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // Forward continue_execution to agent via IPC
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_, session)) = mgr.find_by_agent_id(&agent_id) {
            let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                from: "http-api".to_string(),
                action: "continue_execution".to_string(),
                params: serde_json::json!({
                    "reason": "user_requested",
                }),
                command: None,
            };
            let pushed = session.push_message(intent).await;
            if !pushed {
                return Err(ApiError::internal("Failed to deliver continue signal to agent"));
            }
        } else {
            return Err(ApiError::service_unavailable(&format!(
                "Agent {} is not yet connected",
                agent_id
            )));
        }
    } else {
        return Err(ApiError::internal("Session manager not available"));
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "continued",
            "agent_id": agent_id,
        })),
    ))
}

// ── S1.14: Session API endpoints ─────────────────────────────────────────

/// Query parameters for session messages endpoint
#[derive(Deserialize)]
pub struct SessionMessagesQuery {
    /// Cursor for pagination (message ID)
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum number of messages to return (default: 50)
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Pagination direction: "forward" or "backward" (default: "backward")
    #[serde(default = "default_direction")]
    pub direction: String,
}

fn default_limit() -> u32 {
    50
}

fn default_direction() -> String {
    "backward".to_string()
}

/// Response for listing sessions
#[derive(Serialize)]
pub struct SessionsListResponse {
    /// List of session summaries
    pub sessions: Vec<SessionInfoResponse>,
}

/// Single session info in the response
#[derive(Serialize)]
pub struct SessionInfoResponse {
    /// Session identifier
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Number of messages in the session
    pub message_count: u32,
    /// Optional session title
    pub title: Option<String>,
}

/// Response for session messages
#[derive(Serialize)]
pub struct SessionMessagesResponse {
    /// Messages in the current page
    pub messages: Vec<MessageEntryResponse>,
    /// Cursor for the next page
    pub cursor: Option<String>,
    /// Whether more messages exist
    pub has_more: bool,
}

/// Single message in the session messages response
#[derive(Serialize)]
pub struct MessageEntryResponse {
    /// Unique message ID
    pub id: String,
    /// ISO 8601 timestamp
    pub ts: String,
    /// Message role
    pub role: String,
    /// Message content
    pub content: String,
    /// Optional metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Response for creating a session
#[derive(Serialize)]
pub struct SessionCreatedResponse {
    /// The newly created session identifier
    pub session_id: String,
}

/// `GET /api/agents/{id}/sessions` — list conversation sessions (S1.14)
///
/// Forwards the query to Runtime via IPC and returns the session list.
pub async fn list_sessions(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<SessionsListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let data = forward_session_query(&state, &agent_id, "list_sessions", serde_json::json!({})).await?;

    let sessions: Vec<SessionInfoResponse> = data.get("sessions")
        .and_then(|v| serde_json::from_value::<Vec<rollball_core::protocol::SessionInfoDto>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|s| SessionInfoResponse {
            session_id: s.session_id,
            created_at: s.created_at,
            message_count: s.message_count,
            title: s.title,
        })
        .collect();

    Ok(Json(SessionsListResponse { sessions }))
}

/// `GET /api/agents/{id}/sessions/{session_id}/messages` — get paginated session messages (S1.14)
///
/// Forwards the query to Runtime via IPC and returns the messages.
pub async fn get_session_messages(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
    Query(query): Query<SessionMessagesQuery>,
) -> Result<Json<SessionMessagesResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let params = serde_json::json!({
        "session_id": session_id,
        "cursor": query.cursor,
        "limit": query.limit,
        "direction": query.direction,
    });

    let data = forward_session_query(&state, &agent_id, "get_session_messages", params).await?;

    // Check for error response from Runtime
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(ApiError::bad_request(error));
    }

    let messages: Vec<MessageEntryResponse> = data.get("messages")
        .and_then(|v| serde_json::from_value::<Vec<rollball_core::protocol::ConversationEntryDto>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|m| MessageEntryResponse {
            id: m.id,
            ts: m.ts,
            role: m.role,
            content: m.content,
            metadata: m.metadata,
        })
        .collect();

    let cursor = data.get("cursor").and_then(|v| v.as_str()).map(|s| s.to_string());
    let has_more = data.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false);

    Ok(Json(SessionMessagesResponse {
        messages,
        cursor,
        has_more,
    }))
}

/// `POST /api/agents/{id}/sessions` — create a new conversation session (S1.14)
///
/// Forwards the request to Runtime via IPC, which creates a new
/// ConversationSession and returns the session_id.
pub async fn create_session(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<(StatusCode, Json<SessionCreatedResponse>), (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let data = forward_session_query(&state, &agent_id, "create_session", serde_json::json!({})).await?;

    // Check for error response from Runtime
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(ApiError::internal(error));
    }

    let session_id = data.get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((
        StatusCode::OK,
        Json(SessionCreatedResponse { session_id }),
    ))
}

/// `POST /api/agents/{id}/sessions/{session_id}/activate` — activate an existing session (S1.14)
///
/// Tells the Runtime to switch its active ConversationSession to the specified
/// existing session. The Runtime will resume the session's JSONL file and
/// subsequent messages will be written to it.
///
/// This is the **only correct way** to switch sessions at runtime. Without it,
/// the frontend can update its own sessionStore but the Runtime keeps writing
/// to the old JSONL file — causing messages to appear in wrong sessions.
pub async fn activate_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let params = serde_json::json!({
        "session_id": session_id,
    });

    let data = forward_session_query(&state, &agent_id, "activate_session", params).await?;

    // Check for error response from Runtime
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(ApiError::internal(error));
    }

    Ok(StatusCode::OK)
}

/// `PUT /api/agents/{id}/sessions/{session_id}/title` — update session title (S1.14)
///
/// Persists the session title to the JSONL metadata via Runtime's
/// `update_session_title` action. This is the canonical way to set a
/// session title that survives frontend refreshes.
#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

pub async fn update_session_title(
    State(state): State<AppState>,
    Path((agent_id, _session_id)): Path<(String, String)>,
    Json(body): Json<UpdateTitleRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let params = serde_json::json!({
        "title": body.title,
    });

    let data = forward_session_query(&state, &agent_id, "update_session_title", params).await?;

    // Check for error response from Runtime
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(ApiError::internal(error));
    }

    Ok(StatusCode::OK)
}

/// `DELETE /api/agents/{id}/sessions/{session_id}` — delete a session
///
/// Deletes the session from the Runtime. If the deleted session is the
/// currently active one, the Runtime will automatically create a new session.
pub async fn delete_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let params = serde_json::json!({
        "session_id": session_id,
    });

    let data = forward_session_query(&state, &agent_id, "delete_session", params).await?;

    // Check for error response from Runtime
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(ApiError::internal(error));
    }

    Ok(Json(data))
}

// ── S1.14: IPC forwarding helpers ──────────────────────────────────────────────

/// Default timeout for waiting for session IPC response (10 seconds)
const SESSION_IPC_TIMEOUT_SECS: u64 = 10;

/// Forward a session query to Runtime via IPC push and wait for the response.
///
/// 1. Creates a oneshot channel and stores the sender in the pending map
/// 2. Pushes IntentReceived with the query action to Runtime
/// 3. Waits for Runtime's IntentSend response ("session_response")
/// 4. Returns the response data
async fn forward_session_query(
    state: &AppState,
    agent_id: &str,
    action: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, Json<ApiError>)> {
    let session_mgr = state.session_mgr.as_ref().ok_or_else(|| {
        ApiError::internal("Session manager not available")
    })?;

    let request_id = format!("sess-{}-{}", action, uuid::Uuid::new_v4());

    // Create oneshot channel for response
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    {
        let mut pending = state.session_pending.lock().await;
        pending.insert(request_id.clone(), tx);
    }

    // Push IntentReceived to Runtime
    let mgr = session_mgr.lock().await;
    let (_, session) = mgr.find_by_agent_id(agent_id).ok_or_else(|| {
        ApiError::service_unavailable(&format!("Agent {} is not yet connected", agent_id))
    })?;

    let mut query_params = params;
    query_params["request_id"] = serde_json::json!(request_id);

    let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
        from: "http-api".to_string(),
        action: action.to_string(),
        params: query_params,
        command: None,
    };

    if !session.push_message(intent).await {
        // Clean up pending request
        let mut pending = state.session_pending.lock().await;
        pending.remove(&request_id);
        return Err(ApiError::internal("Failed to deliver session query to agent"));
    }
    drop(mgr); // Release lock before awaiting

    // Wait for response with timeout
    match tokio::time::timeout(
        std::time::Duration::from_secs(SESSION_IPC_TIMEOUT_SECS),
        rx,
    ).await {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(_)) => {
            Err(ApiError::internal("Session response channel closed unexpectedly"))
        }
        Err(_) => {
            // Timeout — clean up pending request
            let mut pending = state.session_pending.lock().await;
            pending.remove(&request_id);
            Err(ApiError::internal("Session query timed out — agent did not respond"))
        }
    }
}

/// Wait for a session response from the pending map.
///
/// Similar to forward_session_query but for cases where the IntentReceived
/// has already been pushed (e.g., in get_conversations/get_latest_conversation).
async fn wait_for_session_response(
    state: &AppState,
    request_id: &str,
) -> Result<serde_json::Value, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    {
        let mut pending = state.session_pending.lock().await;
        pending.insert(request_id.to_string(), tx);
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(SESSION_IPC_TIMEOUT_SECS),
        rx,
    ).await {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(_)) => Err("Session response channel closed".to_string()),
        Err(_) => {
            let mut pending = state.session_pending.lock().await;
            pending.remove(request_id);
            Err("Session query timed out".to_string())
        }
    }
}

/// Parse an ISO 8601 timestamp to Unix epoch seconds.
///
/// Returns 0 if parsing fails.
fn parse_iso8601_to_unix(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

// ── Legacy Grafeo-based conversation queries ──────────────────────────────────

/// Legacy implementation of get_conversations using Grafeo memory store.
async fn get_conversations_legacy(
    state: &AppState,
    agent_id: &str,
) -> Vec<ConversationSummary> {
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    match memory_store {
        Some(store) => match store.get_episodes(None, 10000) {
            Ok(episodes) => {
                let prefix = format!("{}#", agent_id);
                let episodes: Vec<_> = episodes
                    .into_iter()
                    .filter(|ep| ep.session_id.starts_with(&prefix))
                    .collect();

                let mut session_map: std::collections::HashMap<
                    String,
                    Vec<rollball_memory::Episode>,
                > = std::collections::HashMap::new();
                for ep in episodes {
                    session_map
                        .entry(ep.session_id.clone())
                        .or_default()
                        .push(ep);
                }

                let mut conversations: Vec<ConversationSummary> = session_map
                    .into_iter()
                    .map(|(session_id, mut eps)| {
                        eps.sort_by_key(|a| a.timestamp);
                        let started_at =
                            eps.first().map(|e| e.timestamp.timestamp()).unwrap_or(0);
                        let last_message_at =
                            eps.last().map(|e| e.timestamp.timestamp()).unwrap_or(0);
                        ConversationSummary {
                            session_id,
                            started_at,
                            message_count: eps.len() as u32,
                            last_message_at,
                        }
                    })
                    .collect();

                conversations.sort_by_key(|b| std::cmp::Reverse(b.last_message_at));
                conversations
            }
            Err(e) => {
                tracing::warn!("Failed to get episodes for agent {}: {}", agent_id, e);
                vec![]
            }
        },
        None => vec![],
    }
}

/// Legacy implementation of get_latest_conversation using Grafeo memory store.
async fn get_latest_conversation_legacy(
    state: &AppState,
    agent_id: &str,
) -> (String, Vec<ConversationMessage>) {
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    match memory_store {
        Some(store) => match store.get_episodes(None, 10000) {
            Ok(episodes) => {
                let prefix = format!("{}#", agent_id);
                let episodes: Vec<_> = episodes
                    .into_iter()
                    .filter(|ep| ep.session_id.starts_with(&prefix))
                    .collect();

                if episodes.is_empty() {
                    return (String::new(), vec![]);
                }

                let mut session_latest: std::collections::HashMap<String, i64> =
                    std::collections::HashMap::new();
                for ep in &episodes {
                    let ts = ep.timestamp.timestamp();
                    let current = session_latest
                        .entry(ep.session_id.clone())
                        .or_insert(ts);
                    if ts > *current {
                        *current = ts;
                    }
                }

                let latest_session = session_latest
                    .into_iter()
                    .max_by_key(|(_, ts)| *ts)
                    .map(|(sid, _)| sid)
                    .unwrap_or_default();

                if latest_session.is_empty() {
                    return (String::new(), vec![]);
                }

                let mut session_eps: Vec<rollball_memory::Episode> = episodes
                    .into_iter()
                    .filter(|ep| ep.session_id == latest_session)
                    .collect();

                session_eps.sort_by(|a, b| {
                    a.turn_index
                        .cmp(&b.turn_index)
                        .then_with(|| a.timestamp.cmp(&b.timestamp))
                });

                let messages: Vec<ConversationMessage> = session_eps
                    .into_iter()
                    .map(|ep| ConversationMessage {
                        role: ep.role,
                        content: ep.content,
                        timestamp: ep.timestamp.timestamp(),
                        turn_index: ep.turn_index,
                    })
                    .collect();

                (latest_session, messages)
            }
            Err(e) => {
                tracing::warn!("Failed to get episodes for agent {}: {}", agent_id, e);
                (String::new(), vec![])
            }
        },
        None => (String::new(), vec![]),
    }
}

/// Push LLMConfigDelivery to a specific agent after a model_switch.
///
/// When the user switches to a different provider (e.g. deepseek → minimax-cn),
/// the Runtime needs a new LLMConfigDelivery so it can rebuild the Provider
/// with the correct protocol type, API key, base URL, etc.
///
/// If the provider is not found in Vault, logs a warning but does not
/// interrupt the model_switch flow (the model name is still updated via IntentReceived).
async fn push_llm_config_on_switch(
    state: &AppState,
    agent_id: &str,
    provider_name: &str,
    model: &str,
) {
    use rollball_core::protocol::GatewayResponse;

    // Read the provider entry from Vault
    let entry = {
        let gw = state.gateway_state.read().await;
        match gw.vault.get_provider(provider_name) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    agent = %agent_id,
                    provider = %provider_name,
                    "No Vault entry for provider, skipping LLMConfigDelivery on model_switch: {}",
                    e
                );
                return;
            }
        }
    };

    // Resolve model capabilities (user-overridden > models.dev / offline data)
    let model_capabilities = if entry.model_capabilities.is_some() {
        entry.model_capabilities.map(rollball_core::protocol::ModelCapabilitiesInfo::from)
    } else {
        crate::http::models_api::lookup_model_capabilities_with_cache(
            &state.models_cache, provider_name, model,
        ).await
    };
    tracing::info!(
        agent = %agent_id,
        provider = %provider_name,
        model = %model,
        has_capabilities = model_capabilities.is_some(),
        "LLMConfigDelivery: resolved model capabilities"
    );

    // Derive protocol type from models.dev npm field (model-level > provider-level)
    let (protocol_type, api_override) =
        crate::http::models_api::lookup_protocol_info_with_cache(
            &state.models_cache, provider_name, Some(model),
        ).await;

    // Model-level api override takes precedence over Vault base_url
    let effective_base_url = api_override.or(entry.base_url);

    // Read max_output_tokens_limit from Gateway config
    let max_output_tokens_limit = state.gateway_state.read().await.config
        .as_ref().map(|c| c.max_output_tokens_limit).unwrap_or(32_768);

    // Find the session and push LLMConfigDelivery
    if let Some(session_mgr) = &state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(agent_id) {
            let push_result = session.push_message(GatewayResponse::LLMConfigDelivery {
                provider: provider_name.to_string(),
                model: Some(model.to_string()),
                api_key: entry.api_key,
                base_url: effective_base_url,
                models: entry.models,
                model_capabilities,
                max_output_tokens_limit,
                protocol_type,
            }).await;
            if push_result {
                tracing::info!(
                    agent = %agent_id,
                    provider = %provider_name,
                    model = %model,
                    "Pushed LLMConfigDelivery on model_switch"
                );
            } else {
                tracing::warn!(
                    agent = %agent_id,
                    "Failed to push LLMConfigDelivery on model_switch (channel closed)"
                );
            }
        } else {
            tracing::warn!(
                agent = %agent_id,
                "No IPC session found for agent, cannot push LLMConfigDelivery on model_switch"
            );
        }
    }
}