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
    extract::{Path, State},
    http::StatusCode,
    Json,
    response::IntoResponse,
    routing::{get, post},
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
/// Returns a list of conversation sessions grouped by session_id,
/// with message counts and time range for each session.
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

    // Retrieve memory store
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    let conversations = match memory_store {
        Some(store) => match store.get_episodes(None, 10000) {
            Ok(episodes) => {
                // Filter episodes by agent_id prefix to avoid cross-agent pollution
                let prefix = format!("{}#", agent_id);
                let episodes: Vec<_> = episodes
                    .into_iter()
                    .filter(|ep| ep.session_id.starts_with(&prefix))
                    .collect();

                // Group episodes by session_id
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
                        eps.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
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

                // Sort by most recent message descending
                conversations.sort_by(|a, b| b.last_message_at.cmp(&a.last_message_at));
                conversations
            }
            Err(e) => {
                tracing::warn!("Failed to get episodes for agent {}: {}", agent_id, e);
                vec![]
            }
        },
        None => vec![],
    };

    Ok(Json(ConversationsListResponse { conversations }))
}

/// `GET /api/agents/:id/conversations/latest` — get the most recent conversation
///
/// Returns the full message history of the latest session,
/// ordered by turn_index ascending.
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

    // Retrieve memory store
    let memory_store = {
        let gw = state.gateway_state.read().await;
        gw.memory_store.clone()
    };

    let (session_id, messages) = match memory_store {
        Some(store) => match store.get_episodes(None, 10000) {
            Ok(episodes) => {
                // Filter episodes by agent_id prefix to avoid cross-agent pollution
                let prefix = format!("{}#", agent_id);
                let episodes: Vec<_> = episodes
                    .into_iter()
                    .filter(|ep| ep.session_id.starts_with(&prefix))
                    .collect();

                if episodes.is_empty() {
                    (String::new(), vec![])
                } else {
                    // Find the session with the most recent message
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
                        (String::new(), vec![])
                    } else {
                        let mut session_eps: Vec<rollball_memory::Episode> = episodes
                            .into_iter()
                            .filter(|ep| ep.session_id == latest_session)
                            .collect();

                        // Sort by turn_index ascending, then timestamp ascending
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
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get episodes for agent {}: {}", agent_id, e);
                (String::new(), vec![])
            }
        },
        None => (String::new(), vec![]),
    };

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

        tracing::info!(agent = %agent_id, model = %model, "Forwarding model_switch to agent");

        let message_id = format!("msg-{}", uuid::Uuid::new_v4());
        let mut pushed_ok = false;
        if let Some(session_mgr) = &state.session_mgr {
            let mgr = session_mgr.lock().await;
            if let Some((_, session)) = mgr.find_by_agent_id(agent_id) {
                let intent = rollball_core::protocol::GatewayResponse::IntentReceived {
                    from: "http-ws".to_string(),
                    action: "model_switch".to_string(),
                    params: serde_json::json!({
                        "model": model,
                        "message_id": message_id,
                    }),
                };
                pushed_ok = session.push_message(intent).await;
            }
        }

        if pushed_ok {
            let ack = serde_json::json!({
                "type": "ack",
                "message_id": message_id,
            });
            let _ = socket.send(Message::Text(ack.to_string().into())).await;
            let confirmed = serde_json::json!({
                "type": "model_confirmed",
                "model": model,
                "agentId": agent_id,
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_message_request_deserialization() {
        let json = r#"{"content": "Hello, agent!"}"#;
        let req: SendMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Hello, agent!");
        assert!(req.conversation_id.is_none());
    }

    #[test]
    fn test_send_message_request_with_conversation_id() {
        let json = r#"{"content": "Hello!", "conversation_id": "conv-123"}"#;
        let req: SendMessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Hello!");
        assert_eq!(req.conversation_id, Some("conv-123".to_string()));
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
