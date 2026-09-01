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
    routing::get,
};

use crate::http::routes::{ApiError, AppState};

/// Build the chat/conversation router
pub fn chat_routes() -> Router<AppState> {
    // ADR-034: All Runtime endpoints are pure reverse-proxy — the
    // Gateway does not re-parse the response body.  Previous versions
    // of `get_conversations` re-mapped Runtime's `/sessions` response
    // into a bespoke `ConversationSummary` DTO, dropping fields like
    // `live_state` and forcing a Runtime schema change to also touch
    // the Gateway.  Frontend only ever called
    // `/api/agents/{id}/conversations/latest` (which already proxies
    // verbatim), so the listing endpoint is removed entirely.
    Router::new().route(
        "/api/agents/{id}/conversations/latest",
        get(get_latest_conversation),
    )
}

// ── Handlers ────────────────────────────────────────────────────────────

/// `GET /api/agents/:id/conversations/latest` — pure reverse-proxy to
/// Runtime's `/sessions/latest`.  The Runtime's response body is
/// forwarded verbatim so the Gateway never has to track Runtime
/// schema changes (status / todos / context_usage all come straight
/// from the Runtime's `get_latest_session` handler).
pub async fn get_latest_conversation(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
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