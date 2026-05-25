//! Ask Question HTTP endpoint
//!
//! Provides the HTTP API that the Desktop App calls when the user
//! answers an ask_user_question prompt. The endpoint pushes a
//! `question_answer` IntentReceived to the Runtime's AgentLoop
//! (unified push architecture, same as approval_decision).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    Router,
};
use serde::{Deserialize, Serialize};

use rollball_core::protocol::GatewayResponse;

use crate::http::routes::{ApiError, AppState};

/// Request body for the question answer endpoint.
#[derive(Debug, Deserialize)]
pub struct QuestionAnswerRequest {
    /// Unique request ID (matches ChunkEvent::AskQuestion)
    pub request_id: String,
    /// The user's answer:
    /// - If they chose a pre-defined option: the option's label
    /// - If they typed free text (via "Other"): their free-text input
    pub answer: String,
    /// Session ID for multi-session routing (explicit pass-through)
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Response body for the question answer endpoint.
#[derive(Debug, Serialize)]
pub struct QuestionAnswerResponse {
    pub request_id: String,
    pub status: String,
}

/// POST /api/agents/:agent_id/question — submit user's answer to an ask_user_question prompt.
///
/// The Desktop App calls this when the user selects an option or types
/// free text in the AskQuestionCard. The handler pushes the answer
/// back to the Runtime via gRPC IntentReceived (question_answer action),
/// which the Runtime's AgentLoop receives as InboundMessage::QuestionAnswer.
async fn handle_question_answer(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<QuestionAnswerRequest>,
) -> Result<Json<QuestionAnswerResponse>, (StatusCode, Json<ApiError>)> {
    let request_id = req.request_id.clone();

    tracing::info!(
        agent_id = %agent_id,
        request_id = %request_id,
        answer_preview = %req.answer.chars().take(80).collect::<String>(),
        "Question answer received from Desktop App"
    );

    // Push question_answer back to Runtime via gRPC (unified push architecture).
    // The Runtime's AgentLoop is blocked in `await_question_answer()`
    // waiting for InboundMessage::QuestionAnswer.
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let grpc_mgr = grpc_mgr.lock().await;
        if let Some((_, session)) = grpc_mgr.find_by_agent_id(&agent_id) {
            let mut params = serde_json::json!({
                "request_id": &request_id,
                "answer": &req.answer,
            });
            // Explicit session_id pass-through for multi-session routing (P0 fix)
            if let Some(ref sid) = req.session_id {
                params["session_id"] = serde_json::json!(sid);
            }
            let pushed = session.push_message(
                GatewayResponse::IntentReceived {
                    from: "http-api".to_string(),
                    action: "question_answer".to_string(),
                    params,
                    command: None,
                },
            ).await;
            if !pushed {
                tracing::warn!(
                    agent_id = %agent_id,
                    request_id = %request_id,
                    "Failed to push question_answer to Runtime — gRPC channel may be closed"
                );
                return Err(ApiError::internal("Failed to deliver answer to Runtime"));
            }
            tracing::info!(
                agent_id = %agent_id,
                request_id = %request_id,
                "Pushed question_answer to Runtime"
            );
        } else {
            tracing::warn!(
                agent_id = %agent_id,
                "No gRPC session found for agent — cannot push question_answer"
            );
            return Err(ApiError::not_found(&format!(
                "No active session for agent '{}'",
                agent_id
            )));
        }
    } else {
        tracing::warn!("No grpc_session_mgr available — cannot push question_answer");
        return Err(ApiError::internal("Gateway session manager not available"));
    }

    Ok(Json(QuestionAnswerResponse {
        request_id,
        status: "delivered".to_string(),
    }))
}

/// Build the question routes for the HTTP router.
pub fn question_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{agent_id}/question", axum::routing::post(handle_question_answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;

    // Note: Full integration tests require a running gRPC session.
    // The question_answer endpoint pushes to Runtime via gRPC,
    // which requires a connected agent. Unit tests here verify
    // the request/response structure only.

    #[test]
    fn test_question_answer_request_deserialize() {
        let json = r#"{"request_id":"q-1","answer":"Option A","session_id":"sess-123"}"#;
        let req: QuestionAnswerRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "q-1");
        assert_eq!(req.answer, "Option A");
        assert_eq!(req.session_id, Some("sess-123".to_string()));
    }

    #[test]
    fn test_question_answer_request_no_session() {
        let json = r#"{"request_id":"q-2","answer":"My custom input"}"#;
        let req: QuestionAnswerRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "q-2");
        assert_eq!(req.answer, "My custom input");
        assert!(req.session_id.is_none());
    }
}
