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
    routing::{delete, get},
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
pub struct VaultKeyEntryResponse {
    pub provider: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Configured default model (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selected models list (may be empty)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// Add key request (supports full provider configuration)
#[derive(Deserialize)]
pub struct AddKeyRequest {
    pub provider: String,
    pub key: String,
    /// Optional base URL override (e.g. "https://api.deepseek.com/v1")
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model for this provider (e.g. "deepseek-chat")
    /// Kept for backward compatibility — prefer using `models` instead.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    /// models[0] is the default/active model. Takes precedence over default_model.
    #[serde(default)]
    pub models: Vec<String>,
}

/// Update key request (supports partial updates — key is optional)
#[derive(Deserialize)]
pub struct UpdateKeyRequest {
    /// API key. If None or empty, the existing key is preserved.
    /// This prevents the masked key_preview from overwriting the real key.
    #[serde(default)]
    pub key: Option<String>,
    /// Optional base URL override (e.g. "https://api.deepseek.com/v1")
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model for this provider (e.g. "deepseek-chat")
    /// Kept for backward compatibility — prefer using `models` instead.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    /// models[0] is the default/active model. Takes precedence over default_model.
    #[serde(default)]
    pub models: Vec<String>,
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
) -> Result<Json<Vec<VaultKeyEntryResponse>>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let entries = gw.vault.list_keys()
        .map_err(|e| ApiError::internal(&format!("Failed to list keys: {}", e)))?;

    let response: Vec<VaultKeyEntryResponse> = entries.iter().map(|k| {
        // Try to get the full provider entry for base_url/default_model/models
        let (base_url, default_model, models) = match gw.vault.get_provider(&k.provider) {
            Ok(entry) => (entry.base_url.clone(), entry.default_model.clone(), entry.models.clone()),
            Err(_) => (None, None, Vec::new()),
        };
        VaultKeyEntryResponse {
            provider: k.provider.clone(),
            key_preview: k.key_preview.clone(),
            base_url,
            default_model,
            models,
        }
    }).collect();

    Ok(Json(response))
}

/// `POST /api/vault/keys` — add a key (with optional base_url and default_model)
pub async fn add_key(
    State(state): State<AppState>,
    Json(body): Json<AddKeyRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    // Validate base_url format if provided
    if let Some(ref url) = body.base_url {
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ApiError::bad_request(
                "base_url must start with http:// or https://"
            ));
        }
    }
    // Validate provider name is not empty
    if body.provider.is_empty() {
        return Err(ApiError::bad_request("provider must not be empty"));
    }
    // Validate API key is not empty
    if body.key.is_empty() {
        return Err(ApiError::bad_request("key must not be empty"));
    }

    let mut gw = state.gateway_state.write().await;
    // Resolve models: prefer `models` field; fallback to `default_model` for backward compat
    let resolved_models = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        vec![]
    };
    gw.vault.store_provider(
        &body.provider,
        body.base_url.as_deref(),
        &resolved_models,
        &body.key,
    ).map_err(|e| ApiError::internal(&format!("Failed to store key: {}", e)))?;
    drop(gw); // Release write lock before hot-push (which acquires read lock)

    // Hot-push LLMConfigDelivery to all connected agents
    // so they pick up the new provider without requiring a Gateway restart.
    hot_push_llm_config(&state).await;

    Ok((StatusCode::CREATED, Json(MessageResponse {
        message: format!("Key stored for provider: {}", body.provider),
    })))
}

/// `DELETE /api/vault/keys/:provider` — delete a key
pub async fn remove_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;
    gw.vault.remove_key(&provider)
        .map_err(|e| ApiError::not_found(&format!("Key not found for provider '{}': {}", provider, e)))?;

    Ok(Json(MessageResponse {
        message: format!("Key removed for provider: {}", provider),
    }))
}

/// `PUT /api/vault/keys/:provider` — update a key (supports partial updates)
///
/// If `key` is not provided or empty, the existing API key is preserved.
/// This prevents the masked key_preview from overwriting the real key
/// when the user only changes base_url or models.
pub async fn update_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateKeyRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    // Validate base_url format if provided
    if let Some(ref url) = body.base_url {
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ApiError::bad_request(
                "base_url must start with http:// or https://"
            ));
        }
    }

    let mut gw = state.gateway_state.write().await;

    // Resolve the API key: use provided key, or preserve existing key if not specified
    let api_key = match body.key {
        Some(ref k) if !k.is_empty() => k.clone(),
        _ => {
            // No new key provided — preserve existing key from Vault
            match gw.vault.get_provider(&provider) {
                Ok(entry) => entry.api_key,
                Err(e) => {
                    return Err(ApiError::not_found(&format!(
                        "Provider '{}' not found in Vault: {}", provider, e
                    )));
                }
            }
        }
    };

    // Remove old entry, store new with full config
    let _ = gw.vault.remove_key(&provider);
    // Resolve models: prefer `models` field; fallback to `default_model` for backward compat
    let resolved_models = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        vec![]
    };
    gw.vault.store_provider(
        &provider,
        body.base_url.as_deref(),
        &resolved_models,
        &api_key,
    ).map_err(|e| ApiError::internal(&format!("Failed to update key: {}", e)))?;
    drop(gw); // Release write lock before hot-push (which acquires read lock)

    // Hot-push LLMConfigDelivery to all connected agents
    hot_push_llm_config(&state).await;

    Ok(Json(MessageResponse {
        message: format!("Key updated for provider: {}", provider),
    }))
}

/// Hot-push LLMConfigDelivery to all connected agents after a vault update.
/// Uses the shared session manager to find authenticated "main" sessions
/// and pushes the resolved LLM config from Vault.
async fn hot_push_llm_config(state: &AppState) {
    use crate::ipc::server::resolve_llm_config_for_agent;
    use rollball_core::protocol::GatewayResponse;

    let session_mgr = match &state.session_mgr {
        Some(mgr) => mgr.clone(),
        None => {
            tracing::warn!("No IPC session manager available, skipping hot-push");
            return;
        }
    };

    // Collect running agent IDs (brief read lock)
    let agent_ids: Vec<String> = {
        let gw = state.gateway_state.read().await;
        gw.running_agents.keys().cloned().collect()
    };

    for agent_id in agent_ids {
        if let Some((provider, model, api_key, base_url, models)) =
            resolve_llm_config_for_agent(&agent_id, &state.gateway_state).await
        {
            let mgr = session_mgr.lock().await;
            if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                let push_result = session.push_message(GatewayResponse::LLMConfigDelivery {
                    provider: provider.clone(),
                    model: model.clone(),
                    api_key: api_key.clone(),
                    base_url: base_url.clone(),
                    models: models.clone(),
                }).await;
                if push_result {
                    tracing::info!(
                        agent = %agent_id,
                        provider = %provider,
                        "Hot-pushed LLMConfigDelivery after vault update"
                    );
                } else {
                    tracing::warn!(
                        agent = %agent_id,
                        "Failed to hot-push LLMConfigDelivery (channel closed)"
                    );
                }
            }
        }
    }
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
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }
    
    #[test]
    fn test_add_key_request_with_full_config() {
        let json = r#"{"provider": "deepseek", "key": "sk-abc", "base_url": "https://api.deepseek.com/v1", "default_model": "deepseek-chat"}"#;
        let req: AddKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "deepseek");
        assert_eq!(req.key, "sk-abc");
        assert_eq!(req.base_url, Some("https://api.deepseek.com/v1".to_string()));
        assert_eq!(req.default_model, Some("deepseek-chat".to_string()));
    }

    #[test]
    fn test_update_key_request_deserialization() {
        let json = r#"{"key": "sk-new-key"}"#;
        let req: UpdateKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new-key".to_string()));
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }
    
    #[test]
    fn test_update_key_request_with_full_config() {
        let json = r#"{"key": "sk-new", "base_url": "https://api.custom.com/v1", "default_model": "custom-model"}"#;
        let req: UpdateKeyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new".to_string()));
        assert_eq!(req.base_url, Some("https://api.custom.com/v1".to_string()));
        assert_eq!(req.default_model, Some("custom-model".to_string()));
    }
}
