//! Vault HTTP API handlers
//!
//! - GET    /api/vault/keys         — list keys (masked)
//! - POST   /api/vault/keys         — add a key
//! - DELETE /api/vault/keys/:provider — delete a key
//! - PUT    /api/vault/keys/:provider — update a key

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{delete, get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the vault router
pub fn vault_routes() -> Router<AppState> {
    Router::new()
        .route("/api/vault/keys", get(list_keys).post(add_key))
        .route("/api/vault/keys/{provider}", delete(remove_key).put(update_key))
}

// ── Response types ────────────────────────────────────────────────────

/// Masked key entry (first 3 + last 3 chars visible)
#[derive(Serialize)]
pub struct VaultKeyEntry {
    pub provider: String,
    pub key_preview: String,
}

/// Add key request
#[derive(Deserialize)]
pub struct AddKeyRequest {
    pub provider: String,
    pub key: String,
}

/// Update key request
#[derive(Deserialize)]
pub struct UpdateKeyRequest {
    pub key: String,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/vault/keys` — list stored keys (masked)
pub async fn list_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<VaultKeyEntry>>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let keys = gw.vault.list_keys()
        .map_err(|e| ApiError::internal(&format!("Failed to list keys: {}", e)))?;

    let entries: Vec<VaultKeyEntry> = keys.iter().map(|k| {
        let preview = if k.key.len() > 6 {
            format!("{}...{}", &k.key[..3], &k.key[k.key.len()-3..])
        } else {
            "***".to_string()
        };
        VaultKeyEntry {
            provider: k.provider.clone(),
            key_preview: preview,
        }
    }).collect();

    Ok(Json(entries))
}

/// `POST /api/vault/keys` — add a key
pub async fn add_key(
    State(state): State<AppState>,
    Json(body): Json<AddKeyRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    gw.vault.store_key(&body.provider, &body.key)
        .map_err(|e| ApiError::internal(&format!("Failed to store key: {}", e)))?;

    Ok((StatusCode::CREATED, Json(MessageResponse {
        message: format!("Key stored for provider: {}", body.provider),
    })))
}

/// `DELETE /api/vault/keys/:provider` — delete a key
pub async fn remove_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    gw.vault.remove_key(&provider)
        .map_err(|e| ApiError::not_found(&format!("Key not found for provider '{}': {}", provider, e)))?;

    Ok(Json(MessageResponse {
        message: format!("Key removed for provider: {}", provider),
    }))
}

/// `PUT /api/vault/keys/:provider` — update a key
pub async fn update_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateKeyRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    // Remove old, store new
    let _ = gw.vault.remove_key(&provider);
    gw.vault.store_key(&provider, &body.key)
        .map_err(|e| ApiError::internal(&format!("Failed to update key: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Key updated for provider: {}", provider),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_key_request_deserialization() {
        let json = r#"{"provider": "openai", "key": "sk-12345"}"#;
        let req: AddKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "openai");
        assert_eq!(req.key, "sk-12345");
    }

    #[test]
    fn test_update_key_request_deserialization() {
        let json = r#"{"key": "sk-new-key"}"#;
        let req: UpdateKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, "sk-new-key");
    }
}
