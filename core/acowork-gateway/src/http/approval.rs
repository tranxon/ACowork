//! Tool approval HTTP endpoint
//!
//! Provides the HTTP API that the Desktop App calls when the user
//! clicks Allow/Deny in the ToolApprovalModal.
//!
//! ADR-033: Proxies approval decisions to Runtime's `POST /sessions/{sid}/approval` endpoint.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Request body for the approval endpoint.
#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    /// Unique request ID (correlates with the approval event)
    pub request_id: String,
    /// User decision: "allow", "deny", or "allow_all_session"
    pub action: String,
    /// Session ID for multi-session routing (required in MQTT mode)
    pub session_id: String,
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
/// ADR-033: Proxies to Runtime's `POST /sessions/{sid}/approval` endpoint.
/// Returns 200 with `{ request_id, action, status: "resolved" }` on success.
async fn handle_approval(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<ApprovalRequest>,
) -> Result<Json<ApprovalResponse>, (StatusCode, Json<ApiError>)> {
    let request_id = req.request_id.clone();

    tracing::info!(
        agent_id = %agent_id,
        request_id = %request_id,
        action = %req.action,
        session_id = %req.session_id,
        "Tool approval received from Desktop App"
    );

    // ADR-033: Proxy to Runtime HTTP POST /sessions/{sid}/approval.
    let path = format!("/sessions/{}/approval", req.session_id);
    let req_body = serde_json::json!({
        "request_id": req.request_id,
        "action": req.action.clone(),
        "session_id": req.session_id,
    });

    crate::http::proxy::send_runtime_json(
        &state,
        &agent_id,
        &path,
        reqwest::Method::POST,
        Some(&req_body),
    ).await?;

    Ok(Json(ApprovalResponse {
        request_id,
        action: req.action,
        status: "resolved".to_string(),
    }))
}

/// Build the approval routes for the HTTP router.
pub fn approval_routes() -> Router<AppState> {
    Router::new().route(
        "/api/agents/{agent_id}/approval",
        axum::routing::post(handle_approval),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-approval-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
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
        let json = r#"{"request_id":"req-2","action":"deny","session_id":"sess-1"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-2");
        assert_eq!(req.action, "deny");
        assert_eq!(req.session_id, "sess-1");
    }

    #[tokio::test]
    async fn test_approval_request_with_session() {
        let json = r#"{"request_id":"req-3","action":"allow","session_id":"sess-1"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-3");
        assert_eq!(req.action, "allow");
        assert_eq!(req.session_id, "sess-1");
    }

    /// Test that ApprovalRequest requires session_id (deserialization fails without it).
    #[tokio::test]
    async fn test_approval_request_requires_session_id() {
        let json = r#"{"request_id":"req-no-session","action":"allow"}"#;
        let result: Result<ApprovalRequest, _> = serde_json::from_str(json);
        assert!(result.is_err(), "session_id is now required (not optional)");
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
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
