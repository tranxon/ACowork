//! Chat/conversation HTTP API handlers
//!
//! Implements the conversation endpoints:
//! - POST /api/agents/:id/message — send a message (fire-and-forget)
//!
//! ADR-033: WebSocket streaming replaced by MQTT topic-based pub/sub.
//! Desktop subscribes to `acowork/agents/{id}/sessions/{sid}/messages/#`
//! for streaming events instead of using WebSocket.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Maximum content length for a single message (32 KB)
const MAX_CONTENT_LENGTH: usize = 32 * 1024;

/// Build the chat/conversation router
pub fn chat_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/message", post(send_message))
        .route("/api/agents/{id}/conversations", get(get_conversations))
        .route(
            "/api/agents/{id}/conversations/latest",
            get(get_latest_conversation),
        )
        .route(
            "/api/agents/{id}/sessions",
            post(create_session),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}/activate",
            post(activate_session),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}/deactivate",
            post(deactivate_session),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}/title",
            put(update_session_title),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}",
            delete(delete_session),
        )
        .route(
            "/api/agents/{id}/sessions/{session_id}/close",
            post(close_session),
        )
        .route("/api/agents/{id}/continue", post(continue_execution))
}

// ── Request/Response types ────────────────────────────────────────────

/// Request body for sending a message
#[derive(Deserialize)]
pub struct SendMessageRequest {
    /// The message content
    pub content: String,
    /// Frontend-generated message ID for dedup (e.g. "msg-{uuid}").
    /// When set, forwarded to Runtime as-is so JSONL entry ID matches
    /// the frontend's optimistic message ID.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Optional conversation ID for multi-turn
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Session ID for multi-session routing (explicit pass-through)
    #[serde(default)]
    pub session_id: Option<String>,
    /// Skill command selected by the user (e.g. "/commit", "/review-pr")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Document IDs to attach to this message (previously uploaded via /api/sessions/{sid}/documents)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_ids: Option<Vec<String>>,
    /// Multimodal content parts (e.g. text + image_url).
    /// When present, providers serialize content as an array instead of a plain string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_parts: Option<Vec<serde_json::Value>>,
    /// Files/selections attached by user (from workspace explorer / editor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_context: Option<Vec<acowork_core::protocol::AttachedContextItem>>,
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

// ── Handlers ──────────────────────────────────────────────────────────

/// `POST /api/agents/:id/message` — send a message to an agent
///
/// Validates the agent exists and is running, then publishes the message
/// via MQTT control command to the Runtime.
pub async fn send_message(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<SendMessageResponse>), (StatusCode, Json<ApiError>)> {
    // Validate agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
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
            return Err(ApiError::bad_request(
                "conversation_id too long (max 128 characters)",
            ));
        }
        if !conv_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ApiError::bad_request(
                "conversation_id contains invalid characters (only alphanumeric, '-', '_' allowed)",
            ));
        }
    }

    // Validate content length
    // Allow empty content when multimodal content_parts are provided (e.g. image-only message)
    if body.content.is_empty() && body.content_parts.is_none() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if body.content.len() > MAX_CONTENT_LENGTH {
        return Err(ApiError::bad_request(&format!(
            "content too long (max {} bytes, got {})",
            MAX_CONTENT_LENGTH,
            body.content.len()
        )));
    }

    // Use frontend-generated message_id when provided (for ID-based dedup).
    // Only fall back to Gateway-generated ID for legacy clients.
    let message_id = body
        .message_id
        .clone()
        .unwrap_or_else(|| format!("msg-{}", uuid::Uuid::new_v4()));

    // ADR-033: MQTT-first delivery — when MQTT Gateway client is available,
    // publish control command directly.
    let delivered = if let Some(ref mqtt) = state.mqtt_gateway_client {
        let sid = body.session_id.clone().unwrap_or_else(|| {
            format!("sess-{}", uuid::Uuid::new_v4())
        });
        let cmd = acowork_core::mqtt_proto::ControlCommand {
            agent_id: agent_id.clone(),
            command: Some(acowork_core::mqtt_proto::control_command::Command::Message(
                acowork_core::mqtt_proto::MessageCommand {
                    agent_id: agent_id.clone(),
                    session_id: sid.clone(),
                    message_id: message_id.clone(),
                    content: body.content.clone(),
                }
            )),
        };
        match mqtt.publish_control_command(&agent_id, cmd).await {
            Ok(_) => {
                tracing::info!(agent_id = %agent_id, message_id = %message_id, "MQTT: message sent to Runtime");
                state.gateway_state.write().await
                    .touch_interaction(&agent_id, chrono::Utc::now());
                true
            }
            Err(e) => {
                tracing::error!(error = %e, "MQTT publish failed");
                return Err(ApiError::internal("Failed to deliver message via MQTT"));
            }
        }
    } else {
        return Err(ApiError::service_unavailable("MQTT transport not available"));
    };

    if !delivered {
        return Err(ApiError::internal("Failed to deliver message to agent"));
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
/// ADR-033: Proxies to Runtime's `GET /sessions` endpoint.
/// Returns session list from the Runtime's local HTTP server.
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
        if !gw.is_running(&agent_id) {
            return Ok(Json(ConversationsListResponse { conversations: vec![] }));
        }
    }

    // ADR-033: Proxy to Runtime HTTP /sessions endpoint.
    let data = crate::http::proxy::fetch_runtime_json(&state, &agent_id, "/sessions").await?;

    let conversations: Vec<ConversationSummary> = data
        .get("sessions")
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|s| ConversationSummary {
            session_id: s.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            started_at: s.get("created_at")
                .and_then(|v| v.as_str())
                .map(|ts| parse_iso8601_to_unix(ts))
                .unwrap_or(0),
            message_count: s.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            last_message_at: s.get("created_at")
                .and_then(|v| v.as_str())
                .map(|ts| parse_iso8601_to_unix(ts))
                .unwrap_or(0),
        })
        .collect();

    Ok(Json(ConversationsListResponse { conversations }))
}

/// Query parameters for the latest conversation endpoint.
///
/// The frontend MUST pass `session_id` explicitly — there is no "current session"
/// concept on the backend. Every request carries its own session routing.
#[derive(Deserialize)]
pub struct LatestConversationQuery {
    /// Required session ID — the frontend tracks which session is selected.
    pub session_id: String,
}

/// `GET /api/agents/:id/conversations/latest?session_id=...` — get the latest conversation
///
/// Requires `session_id` query param. Returns an error if it is missing or empty,
/// because the backend no longer tracks a "current session".
pub async fn get_latest_conversation(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<LatestConversationQuery>,
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

    if query.session_id.is_empty() {
        return Err(ApiError::bad_request(
            "session_id query parameter is required and must not be empty",
        ));
    }

    // ADR-033: Proxy to Runtime HTTP /sessions/{sid}/messages endpoint.
    let path = format!("/sessions/{}/messages", query.session_id);
    let data = crate::http::proxy::fetch_runtime_json(&state, &agent_id, &path).await?;

    let messages: Vec<ConversationMessage> = data
        .get("messages")
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, m)| ConversationMessage {
            role: m.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            content: m.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            timestamp: m.get("ts").and_then(|v| v.as_str()).map(|ts| parse_iso8601_to_unix(ts)).unwrap_or(0),
            turn_index: i as u32,
        })
        .collect();

    Ok(Json(LatestConversationResponse {
        session_id: query.session_id,
        messages,
    }))
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
    fn test_content_length_limit() {
        // 32KB is the limit
        assert_eq!(MAX_CONTENT_LENGTH, 32 * 1024);
    }

    #[test]
    fn test_conversation_id_valid_format() {
        // Valid formats
        let valid_ids = ["conv-123", "abc_def", "ABC123", "conv-2024-01-01"];
        for id in &valid_ids {
            assert!(
                id.chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            );
        }
    }

    #[test]
    fn test_conversation_id_invalid_chars() {
        // Invalid: contains spaces, dots, slashes
        let invalid_ids = ["conv 123", "conv.123", "conv/123", "conv@123"];
        for id in &invalid_ids {
            assert!(
                !id.chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            );
        }
    }
}

// ── Continue Execution API ────────────────────────────────────────────

/// Request body for continue execution
#[derive(Deserialize)]
pub struct ContinueExecutionRequest {
    /// Optional session ID for multi-session routing
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Continue agent execution after iteration limit was reached.
///
/// ADR-033: Proxies to Runtime's `POST /sessions/{sid}/continue` endpoint.
pub async fn continue_execution(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<ContinueExecutionRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiError>)> {
    // Validate agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-033: session_id is required for MQTT multi-session routing.
    let session_id = match &body.session_id {
        Some(sid) if !sid.is_empty() => sid.as_str(),
        _ => {
            return Err(ApiError::bad_request(
                "session_id is required for continue_execution in MQTT mode",
            ));
        }
    };
    let path = format!("/sessions/{}/continue", session_id);
    let req_body = serde_json::json!({
        "reason": "user_requested",
        "session_id": session_id,
    });

    let data = crate::http::proxy::send_runtime_json(
        &state,
        &agent_id,
        &path,
        reqwest::Method::POST,
        Some(&req_body),
    ).await?;

    Ok((StatusCode::OK, Json(data)))
}

// ── S1.14: Session API endpoints ─────────────────────────────────────────

/// Response for creating a session
#[derive(Serialize)]
pub struct SessionCreatedResponse {
    /// The newly created session identifier
    pub session_id: String,
}

/// `POST /api/agents/{id}/sessions` — create a new conversation session (S1.14)
///
/// ADR-033: Creates session via MQTT CreateSession control command.
/// The session ID is generated by the Gateway.
pub async fn create_session(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<SessionCreatedResponse>), (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-033: MQTT-first session creation.
    let session_id = format!("sess-{}", uuid::Uuid::new_v4());

    if let Some(ref mqtt) = state.mqtt_gateway_client {
        let cmd = acowork_core::mqtt_proto::ControlCommand {
            agent_id: agent_id.clone(),
            command: Some(acowork_core::mqtt_proto::control_command::Command::CreateSession(
                acowork_core::mqtt_proto::CreateSessionCommand {
                    agent_id: agent_id.clone(),
                }
            )),
        };
        let _ = mqtt.publish_control_command(&agent_id, cmd).await;
        tracing::info!(agent_id = %agent_id, session_id = %session_id, "MQTT: session created");
    } else {
        // ADR-033: Without MQTT, the session is created lazily on first message.
        tracing::info!(agent_id = %agent_id, session_id = %session_id, "Session created (lazy, no MQTT)");
    }

    Ok((StatusCode::OK, Json(SessionCreatedResponse { session_id })))
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
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-033: MQTT — session activation is handled lazily by Runtime on first message.
    let _ = (state, agent_id, session_id);
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// `POST /api/agents/{id}/sessions/{session_id}/deactivate`
///
/// ADR-033: MQTT — deactivation is a client-side concept.
/// The frontend stops listening to the session's MQTT topic.
/// No Runtime notification needed; fire-and-forget.
pub async fn deactivate_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let _ = (state, agent_id, session_id);
    Ok(StatusCode::OK)
}

/// `PUT /api/agents/{id}/sessions/{session_id}/title` — update session title (S1.14)
///
/// ADR-033: Proxies to Runtime's `PUT /sessions/{sid}/title` endpoint.
/// The Runtime persists the title to JSONL metadata.
#[derive(Deserialize)]
pub struct UpdateTitleRequest {
    pub title: String,
}

pub async fn update_session_title(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
    Json(body): Json<UpdateTitleRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-033: Proxy to Runtime HTTP PUT /sessions/{sid}/title.
    let path = format!("/sessions/{}/title", session_id);
    let req_body = serde_json::json!({"title": body.title});

    crate::http::proxy::send_runtime_json(
        &state,
        &agent_id,
        &path,
        reqwest::Method::PUT,
        Some(&req_body),
    ).await?;

    Ok(StatusCode::OK)
}

/// `POST /api/agents/{id}/sessions/{session_id}/close` — close a session
///
/// ADR-033: MQTT — close is handled lazily; the Runtime processes
/// delete_session via MQTT control command when `DELETE` is used.
/// Close without delete is a no-op at the Gateway level.
pub async fn close_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let _ = (state, agent_id, session_id);
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// `DELETE /api/agents/{id}/sessions/{session_id}` — delete a session
///
/// ADR-033: Publishes MQTT DeleteSession control command to the Runtime,
/// which handles JSONL cleanup and resource teardown.
pub async fn delete_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-033: MQTT — delete via MQTT DeleteSession control command.
    let mqtt = state.mqtt_gateway_client.as_ref().ok_or_else(|| {
        ApiError::service_unavailable("MQTT transport not available")
    })?;

    let cmd = acowork_core::mqtt_proto::ControlCommand {
        agent_id: agent_id.clone(),
        command: Some(acowork_core::mqtt_proto::control_command::Command::DeleteSession(
            acowork_core::mqtt_proto::DeleteSessionCommand {
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
            }
        )),
    };
    let _ = mqtt.publish_control_command(&agent_id, cmd).await;
    tracing::info!(agent_id = %agent_id, session_id = %session_id, "MQTT: session deleted");

    // Clean up session documents
    cleanup_session_docs(&state, &session_id).await;

    Ok(Json(serde_json::json!({"status": "ok", "deleted": true})))
}

// ── S1.14: gRPC forwarding helpers ──
//
// ADR-033: All gRPC forwarding helpers removed — MQTT replaces gRPC
// for push operations, Runtime HTTP proxy replaces gRPC for queries.

/// Clean up session documents directory (best-effort).
async fn cleanup_session_docs(state: &AppState, session_id: &str) {
    let data_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    };
    let docs_dir = data_dir.join("sessions").join(session_id).join("documents");
    if docs_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&docs_dir) {
            tracing::warn!(session_id = %session_id, error = %e, "Failed to clean up session documents");
        } else {
            tracing::info!(session_id = %session_id, "Cleaned up session documents");
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

// ── Document resolution helper ───────────────────────────────────────

/// Resolve document IDs to their metadata.
///
/// Reads the `.meta.json` files stored alongside the uploaded documents.
/// Returns `None` if the session documents directory doesn't exist.
#[allow(dead_code)]
async fn resolve_document_refs(
    state: &AppState,
    session_id: &str,
    doc_ids: &[String],
) -> Option<serde_json::Value> {
    let data_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    };
    let docs_dir = data_dir.join("sessions").join(session_id).join("documents");

    if !docs_dir.exists() {
        return None;
    }

    let mut docs = Vec::new();
    for doc_id in doc_ids {
        let entries = match std::fs::read_dir(&docs_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem != doc_id {
                continue;
            }
            // Match upload-side naming: {safe_name}.meta.json
            // (e.g. 九年级春季学习计划.docx.meta.json, NOT 九年级春季学习计划.meta.json)
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let meta_path = docs_dir.join(format!("{}.meta.json", filename));
            if let Ok(meta_bytes) = std::fs::read_to_string(&meta_path)
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_bytes)
            {
                docs.push(serde_json::json!({
                    "id": doc_id,
                    "filename": meta.get("filename").and_then(|v| v.as_str()).unwrap_or(""),
                    "abs_path": meta.get("abs_path").and_then(|v| v.as_str()).unwrap_or(""),
                    "format": meta.get("format").and_then(|v| v.as_str()).unwrap_or(""),
                    "size": meta.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                }));
            }
            break;
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(docs))
    }
}

