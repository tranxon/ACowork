//! Tool approval HTTP endpoint
//!
//! Provides the HTTP API that the Desktop App calls when the user
//! clicks Allow/Deny in the ToolApprovalModal. The endpoint resolves
//! a oneshot channel that unblocks the gRPC dispatch handler, which
//! in turn returns the approval result to the Runtime's ApprovalGate.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

use crate::http::routes::{ApiError, AppState};

/// Pending approval request map (shared between HTTP API and gRPC dispatch).
///
/// When Runtime requests tool approval via gRPC IntentSend(action="tool_approval_needed"),
/// the gRPC dispatch handler creates a oneshot channel and stores it here keyed by
/// request_id. The HTTP endpoint `POST /api/agents/:id/approval` resolves the oneshot
/// with the user's decision (Allow/Deny).
pub type ApprovalPendingRequests =
    Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalResult>>>>;

/// Decision sent from the Desktop App after user interaction.
#[derive(Debug, Clone)]
pub struct ApprovalResult {
    /// "allow" or "deny"
    pub action: String,
}

/// Request body for the approval endpoint.
#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    /// Unique request ID (correlates with the oneshot channel)
    pub request_id: String,
    /// User decision: "allow", "deny", or "allow_all_session"
    pub action: String,
}

/// Response body for the approval endpoint.
#[derive(Debug, Serialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub action: String,
    pub status: String,
}

/// Maximum time (seconds) to wait for Runtime to pick up the approval result.
const APPROVAL_TIMEOUT_SECS: u64 = 60;

/// POST /api/agents/:agent_id/approval — resolve a pending tool approval request.
///
/// The Desktop App calls this when the user clicks Allow or Deny in the
/// ToolApprovalModal. The handler looks up the oneshot channel for the given
/// request_id and sends the user's decision, which unblocks the gRPC dispatch
/// handler waiting for this approval.
///
/// Returns 200 with `{ request_id, action, status: "resolved" }` on success,
/// or 404 if the request_id is not found (already resolved or timed out).
async fn handle_approval(
    Path(_agent_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<ApprovalRequest>,
) -> Result<Json<ApprovalResponse>, (StatusCode, Json<ApiError>)> {
    let request_id = req.request_id.clone();

    tracing::info!(
        agent_id = %_agent_id,
        request_id = %request_id,
        action = %req.action,
        "Tool approval received from Desktop App"
    );

    // Look up the oneshot sender for this request_id
    let sender = {
        let mut map = state.approval_pending.lock().await;
        map.remove(&request_id)
    };

    match sender {
        Some(sender) => {
            let action = req.action.clone();
            // Resolve the oneshot — this unblocks the gRPC dispatch handler
            if sender.send(ApprovalResult {
                action: action.clone(),
            }).is_err() {
                tracing::warn!(
                    request_id = %request_id,
                    "Approval oneshot receiver already dropped — Runtime may have timed out"
                );
            }

            tracing::info!(
                request_id = %request_id,
                action = %action,
                "Tool approval resolved successfully"
            );

            Ok(Json(ApprovalResponse {
                request_id,
                action,
                status: "resolved".to_string(),
            }))
        }
        None => {
            tracing::warn!(
                request_id = %request_id,
                "Approval request not found — may have already timed out or been resolved"
            );
            Err(ApiError::not_found(&format!(
                "Approval request '{}' not found (already resolved or timed out)",
                request_id
            )))
        }
    }
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
    use tokio::sync::{Mutex, RwLock};
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rollball-test-approval-{}-{}", std::process::id(), unique));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        let mut state = AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
            None,
            None,
            None,
        );
        state.approval_pending = Arc::new(Mutex::new(HashMap::new()));
        state
    }

    #[tokio::test]
    async fn test_approval_resolve_success() {
        let state = test_app_state();
        let request_id = "test-req-1".to_string();

        // Simulate gRPC dispatch: create a oneshot channel
        let (tx, mut rx) = oneshot::channel::<ApprovalResult>();
        {
            let mut map = state.approval_pending.lock().await;
            map.insert(request_id.clone(), tx);
        }

        // Call the HTTP endpoint
        let result = handle_approval(
            Path("com.test.agent".to_string()),
            State(state.clone()),
            Json(ApprovalRequest {
                request_id: request_id.clone(),
                action: "allow".to_string(),
            }),
        ).await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.request_id, request_id);
        assert_eq!(resp.action, "allow");
        assert_eq!(resp.status, "resolved");

        // Verify the oneshot was resolved
        let approval_result = rx.try_recv().unwrap();
        assert_eq!(approval_result.action, "allow");
    }

    #[tokio::test]
    async fn test_approval_resolve_not_found() {
        let state = test_app_state();

        let result = handle_approval(
            Path("com.test.agent".to_string()),
            State(state),
            Json(ApprovalRequest {
                request_id: "nonexistent".to_string(),
                action: "deny".to_string(),
            }),
        ).await;

        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_approval_resolve_deny() {
        let state = test_app_state();
        let request_id = "test-req-deny".to_string();

        let (tx, mut rx) = oneshot::channel::<ApprovalResult>();
        {
            let mut map = state.approval_pending.lock().await;
            map.insert(request_id.clone(), tx);
        }

        let result = handle_approval(
            Path("com.test.agent".to_string()),
            State(state),
            Json(ApprovalRequest {
                request_id: request_id.clone(),
                action: "deny".to_string(),
            }),
        ).await;

        assert!(result.is_ok());

        let approval_result = rx.try_recv().unwrap();
        assert_eq!(approval_result.action, "deny");
    }
}
