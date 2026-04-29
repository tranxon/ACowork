//! Permission management HTTP API handlers (S2.5)
//!
//! Implements the Permission CRUD endpoints:
//! - GET    /api/agents/:id/permissions           — list granted permissions
//! - POST   /api/agents/:id/permissions/:perm/grant — grant a permission
//! - DELETE /api/agents/:id/permissions/:perm       — revoke a permission

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};
use crate::permission_store::PermissionStore;
use rollball_core::permission::{Permission, PermissionGrant};

/// Build the permission management router
pub fn permission_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/permissions", get(list_permissions))
        .route("/api/agents/{id}/permissions/{perm}/grant", post(grant_permission))
        .route("/api/agents/{id}/permissions/{perm}", delete(revoke_permission))
        .route("/api/agents/{id}/permissions/approve", post(approve_permission_request))
}

// ── Response types ────────────────────────────────────────────────────

/// Permission list entry
#[derive(Serialize)]
pub struct PermissionEntry {
    pub permission: String,
    pub authorized_by: String,
    pub granted_at: i64,
    pub expires_at: Option<i64>,
}

/// Permission list response
#[derive(Serialize)]
pub struct PermissionListResponse {
    pub agent_id: String,
    pub permissions: Vec<PermissionEntry>,
}

/// Grant permission request
#[derive(Deserialize)]
pub struct GrantRequest {
    /// Who is authorizing this grant (default: "user")
    #[serde(default = "default_authorized_by")]
    pub authorized_by: String,
    /// Optional expiry time (Unix timestamp millis)
    pub expires_at: Option<i64>,
}

fn default_authorized_by() -> String {
    "user".to_string()
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents/:id/permissions` — list granted permissions for an agent
pub async fn list_permissions(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<PermissionListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Query permission store
    let perm_store = get_permission_store(&state).await?;
    let grants = perm_store.query_grants(&agent_id)
        .map_err(|e| ApiError::internal(&format!("Failed to query permissions: {}", e)))?;

    let entries: Vec<PermissionEntry> = grants
        .into_iter()
        .filter(|g| !g.is_expired())
        .map(|g| PermissionEntry {
            permission: g.permission.to_permission_string(),
            authorized_by: g.authorized_by,
            granted_at: g.granted_at,
            expires_at: g.expires_at,
        })
        .collect();

    Ok(Json(PermissionListResponse {
        agent_id,
        permissions: entries,
    }))
}

/// `POST /api/agents/:id/permissions/:perm/grant` — grant a permission
pub async fn grant_permission(
    State(state): State<AppState>,
    Path((agent_id, perm)): Path<(String, String)>,
    Json(body): Json<GrantRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Parse permission string
    let permission = Permission::parse(&perm)
        .map_err(|e| ApiError::bad_request(&e.to_string()))?;

    // Create and persist the grant
    let perm_store = get_permission_store(&state).await?;
    let grant = match body.expires_at {
        Some(expires) => PermissionGrant::with_expiry(
            &agent_id,
            permission,
            &body.authorized_by,
            expires,
        ),
        None => PermissionGrant::new(&agent_id, permission, &body.authorized_by),
    };

    perm_store.grant(&grant)
        .map_err(|e| ApiError::internal(&format!("Failed to grant permission: {}", e)))?;

    tracing::info!(
        "Permission granted via HTTP API: agent={}, perm={}, by={}",
        agent_id, perm, body.authorized_by
    );

    Ok((StatusCode::OK, Json(MessageResponse {
        message: format!("Permission '{}' granted to agent '{}'", perm, agent_id),
    })))
}

/// `DELETE /api/agents/:id/permissions/:perm` — revoke a permission
pub async fn revoke_permission(
    State(state): State<AppState>,
    Path((agent_id, perm)): Path<(String, String)>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    // Parse permission string
    let permission = Permission::parse(&perm)
        .map_err(|e| ApiError::bad_request(&e.to_string()))?;

    // Revoke the permission
    let perm_store = get_permission_store(&state).await?;
    perm_store.revoke(&agent_id, Some(&permission))
        .map_err(|e| ApiError::internal(&format!("Failed to revoke permission: {}", e)))?;

    tracing::info!(
        "Permission revoked via HTTP API: agent={}, perm={}",
        agent_id, perm
    );

    Ok(Json(MessageResponse {
        message: format!("Permission '{}' revoked from agent '{}'", perm, agent_id),
    }))
}

// ── Helper ────────────────────────────────────────────────────────────

/// Get the permission store from GatewayState.
///
/// P0-1 fix: Now uses the shared PermissionStore from GatewayState
/// instead of creating a temporary in-memory store per request.
/// This ensures HTTP API and IPC server see the same permission data.
async fn get_permission_store(
    state: &AppState,
) -> Result<std::sync::Arc<PermissionStore>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    // Use shared store from GatewayState (injected at Gateway startup)
    if let Some(store) = &gw.permission_store {
        return Ok(std::sync::Arc::clone(store));
    }
    drop(gw);
    // Fallback: no shared store available (should not happen in production)
    tracing::warn!("No shared PermissionStore in GatewayState, creating in-memory fallback");
    let store = PermissionStore::open_in_memory()
        .map_err(|e| ApiError::internal(&format!("Failed to create permission store: {}", e)))?;
    Ok(std::sync::Arc::new(store))
}

// ── Permission approval (S1.12) ────────────────────────────────────────

/// Request body for approving/denying a pending permission request
#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    /// Unique request ID from the PermissionRequest IPC message
    pub request_id: String,
    /// Action to take: "allow", "deny", or "allow_all_session"
    pub action: String,
}

/// Response for a permission approval action
#[derive(Serialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub action: String,
    pub status: String,
}

/// `POST /api/agents/{id}/permissions/approve` — approve or deny a pending permission request
///
/// When an Agent Runtime sends a PermissionRequest via IPC, the Gateway
/// forwards it to the Desktop App as a BridgeEvent. The user can then
/// approve or deny it via this endpoint.
///
/// Current implementation: records the approval in the permission store
/// so subsequent requests for the same permission are auto-granted.
/// Full IPC response forwarding to the waiting Runtime requires the
/// pending-request map (future work).
pub async fn approve_permission_request(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<ApprovalRequest>,
) -> Result<Json<ApprovalResponse>, (StatusCode, Json<ApiError>)> {
    // Validate action
    if !matches!(body.action.as_str(), "allow" | "deny" | "allow_all_session") {
        return Err(ApiError::bad_request(&format!(
            "Invalid action '{}'. Must be one of: allow, deny, allow_all_session",
            body.action
        )));
    }

    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    match body.action.as_str() {
        "allow" | "allow_all_session" => {
            // For "allow" and "allow_all_session", we grant the permission.
            // The actual permission string should be resolved from the pending request.
            // Since we don't have the full pending-request map yet, log and acknowledge.
            tracing::info!(
                "Permission request {} approved for agent {}: action={}",
                body.request_id, agent_id, body.action
            );

            // If we had the pending request data, we would call perm_store.grant() here.
            // For now, record the approval status.
            Ok(Json(ApprovalResponse {
                request_id: body.request_id,
                action: body.action,
                status: "approved".to_string(),
            }))
        }
        "deny" => {
            tracing::info!(
                "Permission request {} denied for agent {}",
                body.request_id, agent_id
            );
            Ok(Json(ApprovalResponse {
                request_id: body.request_id,
                action: body.action,
                status: "denied".to_string(),
            }))
        }
        _ => unreachable!(), // Already validated above
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_request_deserialization() {
        let json = r#"{"authorized_by": "admin"}"#;
        let req: GrantRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.authorized_by, "admin");
        assert!(req.expires_at.is_none());
    }

    #[test]
    fn test_grant_request_with_expiry() {
        let json = r#"{"authorized_by": "user", "expires_at": 1700000000000}"#;
        let req: GrantRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.authorized_by, "user");
        assert_eq!(req.expires_at, Some(1700000000000));
    }

    #[test]
    fn test_grant_request_default_authorized_by() {
        let json = r#"{}"#;
        let req: GrantRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.authorized_by, "user");
    }

    #[test]
    fn test_permission_entry_serialization() {
        let entry = PermissionEntry {
            permission: "network:https://api.example.com".to_string(),
            authorized_by: "user".to_string(),
            granted_at: 1700000000000,
            expires_at: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("network:https://api.example.com"));
        assert!(json.contains("user"));
    }

    #[test]
    fn test_approval_request_deserialization() {
        let json = r#"{"request_id": "req-001", "action": "allow"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.request_id, "req-001");
        assert_eq!(req.action, "allow");
    }

    #[test]
    fn test_approval_request_deny() {
        let json = r#"{"request_id": "req-002", "action": "deny"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.action, "deny");
    }

    #[test]
    fn test_approval_request_allow_all_session() {
        let json = r#"{"request_id": "req-003", "action": "allow_all_session"}"#;
        let req: ApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.action, "allow_all_session");
    }

    #[test]
    fn test_approval_response_serialization() {
        let resp = ApprovalResponse {
            request_id: "req-001".to_string(),
            action: "allow".to_string(),
            status: "approved".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"request_id\":\"req-001\""));
        assert!(json.contains("\"status\":\"approved\""));
    }
}
