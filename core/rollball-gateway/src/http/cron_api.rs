//! Cron management HTTP API handlers (S3.4)
//!
//! Implements the Cron CRUD endpoints:
//! - GET    /api/agents/:id/cron           — list cron entries for an agent
//! - POST   /api/agents/:id/cron           — register a new cron entry
//! - DELETE /api/agents/:id/cron/:cron_id  — remove a cron entry

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the cron management router
pub fn cron_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/cron", get(list_crons))
        .route("/api/agents/{id}/cron", post(add_cron))
        .route("/api/agents/{id}/cron/{cron_id}", delete(remove_cron))
}

// ── Response types ────────────────────────────────────────────────────

/// Cron entry in API response
#[derive(Serialize)]
pub struct CronEntryResponse {
    pub id: String,
    pub agent_id: String,
    pub schedule: String,
    pub action: String,
    pub params: serde_json::Value,
}

/// Cron list response
#[derive(Serialize)]
pub struct CronListResponse {
    pub agent_id: String,
    pub entries: Vec<CronEntryResponse>,
}

/// Add cron request
#[derive(Deserialize)]
pub struct AddCronRequest {
    /// Cron schedule expression (5-field)
    pub schedule: String,
    /// Action to fire when the schedule triggers
    pub action: String,
    /// Params to include in the IntentReceived
    #[serde(default = "default_params")]
    pub params: serde_json::Value,
}

fn default_params() -> serde_json::Value {
    serde_json::json!({})
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents/:id/cron` — list cron entries for an agent
pub async fn list_crons(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<CronListResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}", agent_id
            )));
        }
    }

    let entries = {
        let gw = state.gateway_state.read().await;
        gw.cron_scheduler
            .entries_for_agent(&agent_id)
            .into_iter()
            .map(|e| CronEntryResponse {
                id: e.id.clone(),
                agent_id: e.agent_id.clone(),
                schedule: e.schedule.clone(),
                action: e.action.clone(),
                params: e.params.clone(),
            })
            .collect()
    };

    Ok(Json(CronListResponse {
        agent_id,
        entries,
    }))
}

/// `POST /api/agents/:id/cron` — register a new cron entry
pub async fn add_cron(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<AddCronRequest>,
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

    // Register in the scheduler
    let cron_id = {
        let mut gw = state.gateway_state.write().await;
        gw.cron_scheduler
            .register(&agent_id, &body.schedule, &body.action, body.params.clone())
            .map_err(|e| ApiError::bad_request(&format!(
                "Invalid cron schedule: {}", e
            )))?
    };

    // Persist to CronStore
    // P1-9 fix: Use spawn_blocking to avoid blocking tokio runtime
    // with synchronous rusqlite operations.
    {
        let store_opt = {
            let gw = state.gateway_state.read().await;
            gw.cron_store.clone()
        };
        if let Some(store) = store_opt {
            let cron_id_clone = cron_id.clone();
            let entry = crate::cron::StoredCronEntry {
                id: cron_id.clone(),
                agent_id: agent_id.clone(),
                schedule: body.schedule.clone(),
                action: body.action.clone(),
                params: serde_json::to_string(&body.params).unwrap_or_else(|_| "{}".to_string()),
            };
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.insert(&entry) {
                    tracing::warn!("Failed to persist cron entry {}: {}", cron_id_clone, e);
                }
            }).await.ok();
        }
    }

    tracing::info!(
        "Cron registered via HTTP API: agent={} cron_id={} schedule={}",
        agent_id, cron_id, body.schedule
    );

    Ok((StatusCode::OK, Json(MessageResponse {
        message: format!("Cron entry '{}' registered for agent '{}'", cron_id, agent_id),
    })))
}

/// `DELETE /api/agents/:id/cron/:cron_id` — remove a cron entry
pub async fn remove_cron(
    State(state): State<AppState>,
    Path((agent_id, cron_id)): Path<(String, String)>,
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

    // Remove from scheduler
    let removed = {
        let mut gw = state.gateway_state.write().await;
        gw.cron_scheduler.unregister(&cron_id)
    };

    if !removed {
        return Err(ApiError::not_found(&format!(
            "Cron entry not found: {}", cron_id
        )));
    }

    // Remove from CronStore (P1-9 fix: spawn_blocking)
    {
        let store_opt = {
            let gw = state.gateway_state.read().await;
            gw.cron_store.clone()
        };
        if let Some(store) = store_opt {
            let cron_id_clone = cron_id.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = store.delete(&cron_id_clone) {
                    tracing::warn!("Failed to delete cron entry {} from store: {}", cron_id_clone, e);
                }
            }).await.ok();
        }
    }

    tracing::info!(
        "Cron removed via HTTP API: agent={} cron_id={}",
        agent_id, cron_id
    );

    Ok(Json(MessageResponse {
        message: format!("Cron entry '{}' removed from agent '{}'", cron_id, agent_id),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_cron_request_deserialization() {
        let json = r#"{"schedule": "0 * * * *", "action": "hourly_check"}"#;
        let req: AddCronRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.schedule, "0 * * * *");
        assert_eq!(req.action, "hourly_check");
        assert!(req.params.is_object());
    }

    #[test]
    fn test_add_cron_request_with_params() {
        let json = r#"{"schedule": "*/15 * * * *", "action": "health", "params": {"type": "ping"}}"#;
        let req: AddCronRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.schedule, "*/15 * * * *");
        assert_eq!(req.params["type"], "ping");
    }

    #[test]
    fn test_cron_entry_response_serialization() {
        let entry = CronEntryResponse {
            id: "cron-1".to_string(),
            agent_id: "com.example.weather".to_string(),
            schedule: "0 * * * *".to_string(),
            action: "hourly_check".to_string(),
            params: serde_json::json!({}),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("cron-1"));
        assert!(json.contains("0 * * * *"));
    }
}
