//! Tool approval HTTP endpoint
//!
//! Provides the HTTP API that the Desktop App calls when the user
//! clicks Allow/Deny in the ToolApprovalModal. The endpoint pushes
//! an `approval_decision` IntentReceived to the Runtime's AgentLoop
//! via gRPC (pure relay — Runtime owns the approval state).

use axum::{
    Json,
    extract::{Path, State},
    Router,
};
use serde::{Deserialize, Serialize};

use rollball_core::protocol::GatewayResponse;

use crate::http::routes::{ApiError, AppState};

/// Request body for the approval endpoint.
#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    /// Unique request ID (correlates with the approval event)
    pub request_id: String,
    /// User decision: "allow", "deny", or "allow_all_session"
    pub action: String,
    /// Session ID for multi-session routing (explicit pass-through)
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Response body for the approval endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub action: String,
    pub status: String,
}

/// POST /api/agents/:agent_id/approval — relay tool approval decision to Runtime.
///
/// The Desktop App calls this when the user clicks Allow or Deny in the
/// ToolApprovalModal. The handler pushes an `approval_decision` IntentReceived
/// to the Runtime via gRPC. Runtime owns the approval state and handles
/// matching by request_id internally.
///
/// Returns 200 with `{ request_id, action, status: "resolved" }` on success.
async fn handle_approval(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<ApprovalRequest>,
) -> Result<Json<ApprovalResponse>, (axum::http::StatusCode, Json<ApiError>)> {
    let request_id = req.request_id.clone();

    tracing::info!(
        agent_id = %agent_id,
        request_id = %request_id,
        action = %req.action,
        "Tool approval received from Desktop App"
    );

    // Push approval_decision back to Runtime via gRPC (pure relay).
    // Runtime owns the approval state; Gateway does NOT maintain approval_pending.
    let approved = req.action == "allow" || req.action == "allow_all_session";
    let allow_all_session = req.action == "allow_all_session";

    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let grpc_mgr = grpc_mgr.lock().await;
        if let Some((_, session)) = grpc_mgr.find_by_agent_id(&agent_id) {
            let mut params = serde_json::json!({
                "request_id": &request_id,
                "approved": approved,
                "allow_all_session": allow_all_session,
            });
            // Explicit session_id pass-through for multi-session routing (P0 fix)
            if let Some(ref sid) = req.session_id {
                params["session_id"] = serde_json::json!(sid);
            }
            let pushed = session.push_message(
                GatewayResponse::IntentReceived {
                    from: "http-api".to_string(),
                    action: "approval_decision".to_string(),
                    params,
                    command: None,
                },
            ).await;
            if !pushed {
                tracing::warn!(
                    agent_id = %agent_id,
                    request_id = %request_id,
                    "Failed to push approval_decision to Runtime — gRPC channel may be closed"
                );
                return Err((
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(ApiError {
                        error: format!("Failed to push approval decision for agent {}", agent_id),
                        code: 503,
                    }),
                ));
            } else {
                tracing::info!(
                    agent_id = %agent_id,
                    request_id = %request_id,
                    approved,
                    allow_all_session,
                    "Pushed approval_decision to Runtime"
                );
            }
        } else {
            tracing::warn!(
                agent_id = %agent_id,
                "No gRPC session found for agent — cannot push approval_decision"
            );
            return Err((
                axum::http::StatusCode::NOT_FOUND,
                Json(ApiError {
                    error: format!("No gRPC session found for agent {}", agent_id),
                    code: 404,
                }),
            ));
        }
    } else {
        tracing::warn!("No grpc_session_mgr available — cannot push approval_decision");
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError {
                error: "gRPC session manager not initialized".to_string(),
                code: 503,
            }),
        ));
    }

    tracing::info!(
        request_id = %request_id,
        action = %req.action,
        "Tool approval relayed to Runtime"
    );

    Ok(Json(ApprovalResponse {
        request_id,
        action: req.action,
        status: "resolved".to_string(),
    }))
}

/// Build the approval routes for the HTTP router.
pub fn approval_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{agent_id}/approval", axum::routing::post(handle_approval))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rollball-test-approval-{}-{}", std::process::id(), unique));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_approval_response_structure() {
        // Verify ApprovalResponse serialization
        let resp = ApprovalResponse {
            request_id: "req-1".to_string(),
            action: "allow".to_string(),
            status: "resolved".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("req-1"));
        assert!(json.contains("allow"));
        assert!(json.contains("resolved"));
    }

    #[tokio::test]
    async fn test_approval_request_deserialization() {
        let json = r#"{"request_id":"req-2","action":"deny"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-2");
        assert_eq!(req.action, "deny");
        assert!(req.session_id.is_none());
    }

    #[tokio::test]
    async fn test_approval_request_with_session() {
        let json = r#"{"request_id":"req-3","action":"allow","session_id":"sess-1"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-3");
        assert_eq!(req.action, "allow");
        assert_eq!(req.session_id, Some("sess-1".to_string()));
    }

    /// Test that handle_approval returns 503 when grpc_session_mgr is None.
    #[tokio::test]
    async fn test_handle_approval_no_grpc_session_mgr() {
        let state = test_app_state();
        assert!(state.grpc_session_mgr.is_none(), "test state must have no grpc_session_mgr");

        let app = approval_routes().with_state(state);

        let body = serde_json::json!({
            "request_id": "req-no-grpc",
            "action": "allow"
        });

        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/test-agent/approval")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let error: ApiError = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(error.code, 503);
        assert!(error.error.contains("gRPC session manager not initialized"));
    }

    /// Test that ApprovalRequest rejects invalid JSON.
    #[tokio::test]
    async fn test_handle_approval_invalid_body() {
        let state = test_app_state();
        let app = approval_routes().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/agents/test-agent/approval")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Test ApprovalDecision reason field presence in serialization.
    /// (ApprovalDecision lives in runtime crate; we test the concept via
    /// ApprovalResponse structure which carries the decision outcome.)
    #[tokio::test]
    async fn test_approval_response_with_reason() {
        // ApprovalResponse doesn't have a reason field directly, but we verify
        // that the serialized form can carry extra fields for forward compat.
        let json = serde_json::json!({
            "request_id": "req-reason",
            "action": "deny",
            "status": "resolved",
            "reason": "tool approval timed out after 300s"
        });
        let resp: ApprovalResponse = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(resp.request_id, "req-reason");
        assert_eq!(resp.action, "deny");
        assert_eq!(resp.status, "resolved");

        // Re-serialize: the extra "reason" field is gracefully ignored on deser
        let re_json = serde_json::to_value(&resp).unwrap();
        assert_eq!(re_json["request_id"], "req-reason");
        assert_eq!(re_json["action"], "deny");
    }
}
