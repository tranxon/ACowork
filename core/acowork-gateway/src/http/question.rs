//! Ask Question HTTP endpoint
//!
//! Provides the HTTP API that the Desktop App calls when the user
//! answers an ask_user_question prompt.
//!
//! ADR-033: Proxies question answers to Runtime's `POST /sessions/{sid}/question` endpoint.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

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
    /// Session ID for multi-session routing (required in MQTT mode)
    pub session_id: String,
}

/// Response body for the question answer endpoint.
#[derive(Debug, Serialize)]
pub struct QuestionAnswerResponse {
    pub request_id: String,
    pub status: String,
}

/// POST /api/agents/:agent_id/question — submit user's answer to an ask_user_question prompt.
///
/// ADR-033: Proxies to Runtime's `POST /sessions/{sid}/question` endpoint.
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
        session_id = %req.session_id,
        "Question answer received from Desktop App"
    );

    // ADR-033: Proxy to Runtime HTTP POST /sessions/{sid}/question.
    let path = format!("/sessions/{}/question", req.session_id);
    let req_body = serde_json::json!({
        "request_id": req.request_id,
        "answer": req.answer,
        "session_id": req.session_id,
    });

    crate::http::proxy::send_runtime_json(
        &state,
        &agent_id,
        &path,
        reqwest::Method::POST,
        Some(&req_body),
    ).await?;

    Ok(Json(QuestionAnswerResponse {
        request_id,
        status: "resolved".to_string(),
    }))
}

/// Build the question routes for the HTTP router.
pub fn question_routes() -> Router<AppState> {
    Router::new().route(
        "/api/agents/{agent_id}/question",
        axum::routing::post(handle_question_answer),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(req.session_id, "sess-123");
    }

    #[test]
    fn test_question_answer_request_no_session() {
        let json = r#"{"request_id":"q-2","answer":"My custom input"}"#;
        let result: Result<QuestionAnswerRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "session_id is now required");
    }
}
