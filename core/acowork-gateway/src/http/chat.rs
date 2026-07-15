//! Chat/conversation HTTP API handlers
//!
//! ADR-034 §7.3 L2 + Phase 9: After Desktop switched to MQTT `chat_message`,
//! the `POST /api/agents/{id}/message` HTTP control path was removed.
//! This module now only retains read-only conversation queries (proxied to
//! Runtime's `/sessions` and `/sessions/latest`).
//!
//! ADR-033: WebSocket streaming replaced by MQTT topic-based pub/sub.
//! Desktop subscribes to `acowork/agents/{id}/sessions/{sid}/messages/#`
//! for streaming events instead of using WebSocket.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use serde::Serialize;

use crate::http::routes::{ApiError, AppState};

/// Build the chat/conversation router
pub fn chat_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/conversations", get(get_conversations))
        .route(
            "/api/agents/{id}/conversations/latest",
            get(get_latest_conversation),
        )
}

// ── Request/Response types ────────────────────────────────────────────

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

// ── Handlers ────────────────────────────────────────────────────────────

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
                .map(parse_iso8601_to_unix)
                .unwrap_or(0),
            message_count: s.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            last_message_at: s.get("created_at")
                .and_then(|v| v.as_str())
                .map(parse_iso8601_to_unix)
                .unwrap_or(0),
        })
        .collect();

    Ok(Json(ConversationsListResponse { conversations }))
}

/// `GET /api/agents/:id/conversations/latest` — proxy to Runtime's `/sessions/latest`.
///
/// The Runtime returns the most recent session metadata. This is the
/// correct use of the "latest" endpoint — the frontend no longer passes
/// a `session_id` query param (that would be "get messages for a session",
/// which is what `/sessions/{sid}/messages` is for).
pub async fn get_latest_conversation(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // ADR-034 §7.1 E1 fix: Proxy to Runtime /sessions/latest instead of /sessions/{sid}/messages.
    let data = crate::http::proxy::fetch_runtime_json(&state, &agent_id, "/sessions/latest").await?;
    Ok(Json(data))
}

/// Parse an ISO 8601 timestamp to Unix epoch seconds.
///
/// Returns 0 if parsing fails.
fn parse_iso8601_to_unix(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_conversation_id_valid_format() {
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
        let invalid_ids = ["conv 123", "conv.123", "conv/123", "conv@123"];
        for id in &invalid_ids {
            assert!(
                !id.chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            );
        }
    }
}