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
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};
use crate::resource_cache;
use crate::vault::StoredModelCapabilities;
use crate::http::models_api;
use std::path::PathBuf;

/// Build the vault router
pub fn vault_routes() -> Router<AppState> {
    Router::new()
        .route("/api/vault/keys", get(list_keys).post(add_key))
        .route("/api/vault/keys/{provider}", delete(remove_key).put(update_key))
        .route("/api/search/keys", get(list_search_keys).post(add_search_key))
        .route("/api/search/keys/{provider}", delete(remove_search_key).put(update_search_key))
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
    /// User-overridden model capabilities per model name
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_capabilities: std::collections::HashMap<String, StoredModelCapabilities>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a local (self-hosted) provider (no API key required)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
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
    /// Per-model capabilities.
    /// Each key is a model ID matching the `models` list.
    #[serde(default)]
    pub model_capabilities: std::collections::HashMap<String, StoredModelCapabilities>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default)]
    pub compact_model: Option<String>,
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
    /// Per-model capabilities (partial update).
    /// When present, each model entry replaces the existing one.
    #[serde(default)]
    pub model_capabilities: std::collections::HashMap<String, StoredModelCapabilities>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default)]
    pub compact_model: Option<String>,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Search key types ──────────────────────────────────────────────────

/// Search key entry response (masked preview)
#[derive(Serialize)]
pub struct SearchKeyEntryResponse {
    pub provider: String,
    pub key_preview: String,
}

/// Add search key request
#[derive(Deserialize)]
pub struct AddSearchKeyRequest {
    pub provider: String,
    pub key: String,
}

/// Update search key request (partial update — key is optional)
#[derive(Deserialize)]
pub struct UpdateSearchKeyRequest {
    #[serde(default)]
    pub key: Option<String>,
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
        // Try to get the full provider entry for base_url/default_model/models/capabilities
        let (base_url, default_model, models, model_capabilities, compact_model) = match gw.vault.get_provider(&k.provider) {
            Ok(entry) => (
                entry.base_url.clone(),
                entry.default_model.clone(),
                entry.models.clone(),
                entry.model_capabilities.clone(),
                entry.compact_model.clone(),
            ),
            Err(_) => (None, None, Vec::new(), std::collections::HashMap::new(), None),
        };
        let is_local = models_api::is_local_provider(&k.provider);
        VaultKeyEntryResponse {
            provider: k.provider.clone(),
            key_preview: if is_local { "(local)".to_string() } else { k.key_preview.clone() },
            base_url,
            default_model,
            models,
            model_capabilities,
            compact_model,
            local: is_local,
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
    if let Some(ref url) = body.base_url
        && !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://"
        ));
    }
    // Validate provider name is not empty
    if body.provider.is_empty() {
        return Err(ApiError::bad_request("provider must not be empty"));
    }
    // Validate API key is not empty (skip for local providers, which don't need keys)
    let is_local = models_api::is_local_provider(&body.provider);
    if !is_local && body.key.is_empty() {
        return Err(ApiError::bad_request("key must not be empty"));
    }
    // Per-model capabilities map; individual validation is done per-entry downstream

    let mut gw = state.gateway_state.write().await;
    // Local providers use a placeholder key (no real API key needed)
    let effective_key = if is_local { "local".to_string() } else { body.key.clone() };
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
        &effective_key,
        &body.model_capabilities,
        body.compact_model.as_deref(),
    ).map_err(|e| ApiError::internal(&format!("Failed to store key: {}", e)))?;

    // Rebuild provider_list cache so AgentHello picks up the new provider.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_provider_cache(&mut gw, &data_dir).await;
    drop(gw); // Release write lock before hot-push (which acquires read lock)

    // Hot-push resource version change to all connected agents
    // so they pick up the new provider without requiring a Gateway restart.
    if let Some(ref pusher) = state.pusher { pusher.push_llm_config().await; }

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

    // Rebuild provider_list cache after removal.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_provider_cache(&mut gw, &data_dir).await;

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
    if let Some(ref url) = body.base_url
        && !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://"
        ));
    }

    // Per-model capabilities map; individual validation downstream

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

    // Resolve models: prefer `models` field; fallback to `default_model` for backward compat;
    // if neither is provided, preserve existing models from Vault
    let resolved_models = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        // Preserve existing models from Vault if not specified in update request
        match gw.vault.get_provider(&provider) {
            Ok(entry) if !entry.models.is_empty() => entry.models.clone(),
            _ => vec![],
        }
    };

    // Resolve base_url: use provided value, or preserve existing if not specified
    let resolved_base_url = if body.base_url.is_some() {
        body.base_url.clone()
    } else {
        match gw.vault.get_provider(&provider) {
            Ok(entry) => entry.base_url.clone(),
            Err(_) => None,
        }
    };

    // Resolve capabilities: use provided value, or preserve existing if not specified
    let resolved_capabilities = if !body.model_capabilities.is_empty() {
        body.model_capabilities.clone()
    } else {
        // Preserve existing capabilities from Vault if not specified in update request
        match gw.vault.get_provider(&provider) {
            Ok(entry) => entry.model_capabilities.clone(),
            Err(_) => std::collections::HashMap::new(),
        }
    };

    // Resolve compact_model: use provided value, or preserve existing if not specified
    let resolved_compact_model = if body.compact_model.is_some() {
        body.compact_model.clone()
    } else {
        match gw.vault.get_provider(&provider) {
            Ok(entry) => entry.compact_model.clone(),
            Err(_) => None,
        }
    };

    // Remove old entry, store new with full config
    let _ = gw.vault.remove_key(&provider);
    gw.vault.store_provider(
        &provider,
        resolved_base_url.as_deref(),
        &resolved_models,
        &api_key,
        &resolved_capabilities,
        resolved_compact_model.as_deref(),
    ).map_err(|e| ApiError::internal(&format!("Failed to update key: {}", e)))?;

    // Rebuild provider_list cache after update.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_provider_cache(&mut gw, &data_dir).await;
    drop(gw); // Release write lock before hot-push (which acquires read lock)

    // Hot-push resource version change to all connected agents
    if let Some(ref pusher) = state.pusher { pusher.push_llm_config().await; }

    Ok(Json(MessageResponse {
        message: format!("Key updated for provider: {}", provider),
    }))
}

// ── Search key handlers ───────────────────────────────────────────────

/// `GET /api/search/keys` — list stored search provider keys (masked)
pub async fn list_search_keys(
    State(state): State<AppState>,
) -> Result<Json<Vec<SearchKeyEntryResponse>>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let entries = gw.vault.list_search_keys()
        .map_err(|e| ApiError::internal(&format!("Failed to list search keys: {}", e)))?;

    let response = entries.iter().map(|k| SearchKeyEntryResponse {
        provider: k.provider.clone(),
        key_preview: k.key_preview.clone(),
    }).collect();

    Ok(Json(response))
}

/// `POST /api/search/keys` — add a search provider API key
pub async fn add_search_key(
    State(state): State<AppState>,
    Json(body): Json<AddSearchKeyRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    if body.provider.is_empty() {
        return Err(ApiError::bad_request("provider must not be empty"));
    }
    if body.key.is_empty() {
        return Err(ApiError::bad_request("key must not be empty"));
    }

    let mut gw = state.gateway_state.write().await;
    gw.vault.store_search_key(&body.provider, &body.key)
        .map_err(|e| ApiError::internal(&format!("Failed to store search key: {}", e)))?;

    // Rebuild search_list cache so AgentHello picks up the new provider.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw); // Release write lock before hot-push

    // Hot-push search config change to all connected agents
    if let Some(ref pusher) = state.pusher { pusher.push_search_config().await; }

    Ok((StatusCode::CREATED, Json(MessageResponse {
        message: format!("Search key stored for provider: {}", body.provider),
    })))
}

/// `DELETE /api/search/keys/:provider` — remove a search provider API key
pub async fn remove_search_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;
    gw.vault.remove_search_key(&provider)
        .map_err(|e| ApiError::not_found(&format!("Search key not found for '{}': {}", provider, e)))?;

    // Rebuild search_list cache after removal.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw);

    if let Some(ref pusher) = state.pusher { pusher.push_search_config().await; }

    Ok(Json(MessageResponse {
        message: format!("Search key removed for provider: {}", provider),
    }))
}

/// `PUT /api/search/keys/:provider` — update a search provider API key (partial)
pub async fn update_search_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateSearchKeyRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    // Resolve the API key: use provided key, or preserve existing key
    let api_key = match body.key {
        Some(ref k) if !k.is_empty() => k.clone(),
        _ => {
            match gw.vault.get_search_key(&provider) {
                Ok(entry) => entry.api_key,
                Err(e) => {
                    return Err(ApiError::not_found(&format!(
                        "Search key not found for '{}': {}", provider, e
                    )));
                }
            }
        }
    };

    // Remove old entry, store new
    let _ = gw.vault.remove_search_key(&provider);
    gw.vault.store_search_key(&provider, &api_key)
        .map_err(|e| ApiError::internal(&format!("Failed to update search key: {}", e)))?;

    // Rebuild search_list cache after update.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw);

    if let Some(ref pusher) = state.pusher { pusher.push_search_config().await; }

    Ok(Json(MessageResponse {
        message: format!("Search key updated for provider: {}", provider),
    }))
}


// ── Helpers ───────────────────────────────────────────────────────────

/// Get data_dir from GatewayState config.
fn get_data_dir_from_gw(gw: &crate::gateway::state::GatewayState) -> PathBuf {
    gw.config
        .as_ref()
        .map(|c| PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| PathBuf::from("./data"))
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
