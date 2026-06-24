//! Provider configuration HTTP API handlers
//!
//! Full provider lifecycle management: API key (encrypted Vault) +
//! configuration (provider_list.json: base_url, models, capabilities, compact_model).
//!
//! - GET    /api/providers          — list providers (masked keys) + config
//! - POST   /api/providers          — add a provider (key + config)
//! - DELETE /api/providers/:provider — remove a provider
//! - PUT    /api/providers/:provider — update a provider (key and/or config)

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};

use crate::http::models_api;
use crate::http::routes::{ApiError, AppState};
use crate::resource_cache;
use acowork_core::protocol::ModelCapabilitiesInfo;
use std::collections::HashMap;
use std::path::PathBuf;

/// Build the provider configuration router
pub fn provider_routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers", get(list_providers).post(add_provider))
        .route(
            "/api/providers/{provider}",
            delete(remove_provider).put(update_provider),
        )
        .route(
            "/api/search/keys",
            get(list_search_keys).post(add_search_key),
        )
        .route(
            "/api/search/keys/{provider}",
            delete(remove_search_key).put(update_search_key),
        )
}

// ── Response types ────────────────────────────────────────────────────

/// Masked key entry with provider config (first 3 + last 3 chars visible).
///
/// Config fields (base_url, models, compact_model) are read from
/// provider_list.json, NOT from Vault.
#[derive(Serialize)]
pub struct ProviderEntryResponse {
    pub provider: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Configured default model (models[0])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selected models list (may be empty)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a local (self-hosted) provider (no API key required)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    /// Whether this is a user-defined custom provider (not listed in models.dev)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
    /// Per-model capabilities map (model ID → capabilities), including user-configured overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
}

/// Default max output tokens when gateway config doesn't specify a limit.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;

/// Add provider request.
///
/// `key` → stored in encrypted Vault.
/// `base_url`, `models`, `compact_model` → stored in provider_list.json.
/// `model_capabilities` → user-configured overrides merged into offline data.
#[derive(Deserialize)]
pub struct AddProviderRequest {
    pub provider: String,
    pub key: String,
    /// Optional base URL override (e.g. "https://api.deepseek.com/v1")
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model (fallback if `models` is empty)
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    /// models[0] is the default/active model.
    #[serde(default)]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default)]
    pub compact_model: Option<String>,
    /// Whether this is a custom (user-defined) provider not listed in models.dev.
    /// Custom providers always use OpenAI-compatible protocol.
    #[serde(default)]
    pub custom: Option<bool>,
    /// Per-model capabilities overrides (model ID → capabilities).
    /// User-configured fields (e.g. `default_reasoning_effort`) are merged
    /// into the offline models.dev data so the Runtime sees user preferences.
    #[serde(default)]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
}

/// Update provider request (supports partial updates — key and config are optional).
#[derive(Deserialize)]
pub struct UpdateProviderRequest {
    /// API key. If None or empty, the existing key is preserved.
    #[serde(default)]
    pub key: Option<String>,
    /// Optional base URL override
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model (fallback if `models` is empty)
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    #[serde(default)]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010).
    #[serde(default)]
    pub compact_model: Option<String>,
    /// Per-model capabilities overrides (model ID → capabilities).
    /// User-configured fields (e.g. `default_reasoning_effort`) are merged
    /// into the offline models.dev data so the Runtime sees user preferences.
    #[serde(default)]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
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

/// `GET /api/providers` — list stored providers (masked keys) with config.
///
/// Key previews come from Vault. Config (base_url, models, compact_model)
/// comes from provider_list.json (resource_cache).
pub async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<ProviderEntryResponse>>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Build key_preview lookup from Vault (only for key masking, not authority).
    // resource_cache.provider_list is the source of truth for which providers exist.
    let key_previews: std::collections::HashMap<String, String> = gw
        .vault
        .list_keys()
        .map(|entries| {
            entries
                .into_iter()
                .map(|e| (e.provider, e.key_preview))
                .collect()
        })
        .unwrap_or_default();

    // Iterate resource_cache as source of truth for which providers exist.
    let response: Vec<ProviderEntryResponse> = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .map(|cfg| {
            let is_local = models_api::is_local_provider(&cfg.id);
            let key_preview = if is_local {
                "(local)".to_string()
            } else {
                key_previews.get(&cfg.id).cloned().unwrap_or_default()
            };
            ProviderEntryResponse {
                provider: cfg.id.clone(),
                key_preview,
                base_url: if cfg.base_url.is_empty() {
                    None
                } else {
                    Some(cfg.base_url.clone())
                },
                default_model: cfg.models.first().map(|m| m.id.clone()),
                models: cfg.models.iter().map(|m| m.id.clone()).collect(),
                compact_model: cfg.compact_model.clone(),
                local: is_local,
                custom: cfg.custom,
                model_capabilities: {
                    let caps: HashMap<String, ModelCapabilitiesInfo> = cfg
                        .models
                        .iter()
                        .map(|m| (m.id.clone(), m.capabilities.clone()))
                        .collect();
                    if caps.is_empty() { None } else { Some(caps) }
                },
            }
        })
        .collect();

    Ok(Json(response))
}

/// `POST /api/providers` — add a provider (key + config).
///
/// API key → stored in encrypted Vault.
/// Config (base_url, models, compact_model, model_capabilities) → built from
/// request + offline capabilities, stored in provider_list.json via resource_cache.
pub async fn add_provider(
    State(state): State<AppState>,
    Json(body): Json<AddProviderRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    // Validate base_url format if provided
    if let Some(ref url) = body.base_url
        && !url.is_empty()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://",
        ));
    }
    if body.provider.is_empty() {
        return Err(ApiError::bad_request("provider must not be empty"));
    }
    let is_local = models_api::is_local_provider(&body.provider);
    let is_custom = body.custom.unwrap_or(false);
    if !is_local && !is_custom && body.key.is_empty() {
        return Err(ApiError::bad_request("key must not be empty"));
    }

    let mut gw = state.gateway_state.write().await;

    // 1. Store API key in encrypted Vault.
    let effective_key = if is_local {
        "local".to_string()
    } else if is_custom && body.key.is_empty() {
        "custom".to_string()
    } else {
        body.key.clone()
    };
    gw.vault
        .store_key(&body.provider, &effective_key)
        .map_err(|e| ApiError::internal(&format!("Failed to store key: {}", e)))?;

    // 2. Resolve models list.
    let resolved_models: Vec<String> = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        vec![]
    };

    // 3. Build ProviderListItem (capabilities from offline_providers.json).
    let max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let mut item = resource_cache::build_provider_list_item(
        &body.provider,
        body.base_url.as_deref(),
        &resolved_models,
        body.compact_model.as_deref(),
        max_output_tokens,
        is_custom,
    );

    // 3b. Merge user-provided model_capabilities overrides (e.g. default_reasoning_effort).
    if let Some(ref user_caps) = body.model_capabilities {
        merge_user_capabilities(&mut item, user_caps);
    }

    // 4. Add to in-memory provider list (replace if already exists).
    resource_cache::remove_provider_from_memory(&mut gw, &body.provider);
    gw.resource_cache
        .provider_list
        .providers
        .push(item);

    // 5. Persist to disk and bump version.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    // 6. Hot-push to running agents.
    if let Some(ref pusher) = state.pusher {
        pusher.push_llm_config().await;
    }

    Ok((
        StatusCode::CREATED,
        Json(MessageResponse {
            message: format!("Key stored for provider: {}", body.provider),
        }),
    ))
}

/// `DELETE /api/providers/:provider` — remove a provider (key + config).
pub async fn remove_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    // 1. Remove API key from Vault.
    gw.vault.remove_key(&provider).map_err(|e| {
        ApiError::not_found(&format!("Key not found for provider '{}': {}", provider, e))
    })?;

    // 2. Remove from in-memory provider list.
    resource_cache::remove_provider_from_memory(&mut gw, &provider);

    // 3. Persist to disk.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    // 4. Hot-push.
    if let Some(ref pusher) = state.pusher {
        pusher.push_llm_config().await;
    }

    Ok(Json(MessageResponse {
        message: format!("Key removed for provider: {}", provider),
    }))
}

/// `PUT /api/providers/:provider` — update a provider (key and/or config).
///
/// If `key` is None/empty, the existing Vault key is preserved.
/// If `models` is empty and `default_model` is None, existing models are
/// preserved from provider_list.json.
pub async fn update_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateProviderRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    if let Some(ref url) = body.base_url
        && !url.is_empty()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://",
        ));
    }

    let mut gw = state.gateway_state.write().await;

    // 1. Update API key in Vault (preserve existing if not provided).
    let api_key = match body.key {
        Some(ref k) if !k.is_empty() => k.clone(),
        _ => match gw.vault.get_provider(&provider) {
            Ok(entry) => entry.api_key,
            Err(e) => {
                return Err(ApiError::not_found(&format!(
                    "Provider '{}' not found in Vault: {}",
                    provider, e
                )));
            }
        },
    };
    // Only re-store if the key actually changed; otherwise skip to avoid
    // unnecessary Vault re-serialization.
    if body.key.as_ref().is_some_and(|k| !k.is_empty()) {
        gw.vault
            .store_key(&provider, &api_key)
            .map_err(|e| ApiError::internal(&format!("Failed to update key: {}", e)))?;
    }

    // 2. Resolve models: provided > default_model > existing from cache.
    let resolved_models: Vec<String> = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        // Preserve existing models from provider_list.json cache.
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map(|p| p.models.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default()
    };

    // 3. Resolve base_url: provided > existing from cache.
    let resolved_base_url = if body.base_url.is_some() {
        body.base_url.clone()
    } else {
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| {
                if p.base_url.is_empty() {
                    None
                } else {
                    Some(p.base_url.clone())
                }
            })
    };

    // 4. Resolve compact_model: provided > existing from cache.
    let resolved_compact_model = if body.compact_model.is_some() {
        body.compact_model.clone()
    } else {
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| p.compact_model.clone())
    };

    // 5. Rebuild ProviderListItem (capabilities from offline_providers.json).
    let max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    // Preserve the existing custom flag from the stored provider entry.
    let is_custom = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider)
        .map(|p| p.custom)
        .unwrap_or(false);
    let mut item = resource_cache::build_provider_list_item(
        &provider,
        resolved_base_url.as_deref(),
        &resolved_models,
        resolved_compact_model.as_deref(),
        max_output_tokens,
        is_custom,
    );

    // 5b. Merge user-provided model_capabilities overrides (e.g. default_reasoning_effort).
    if let Some(ref user_caps) = body.model_capabilities {
        merge_user_capabilities(&mut item, user_caps);
    }

    // 6. Replace in in-memory list.
    resource_cache::remove_provider_from_memory(&mut gw, &provider);
    gw.resource_cache
        .provider_list
        .providers
        .push(item);

    // 7. Persist to disk.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    // 8. Hot-push.
    if let Some(ref pusher) = state.pusher {
        pusher.push_llm_config().await;
    }

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
    let entries = gw
        .vault
        .list_search_keys()
        .map_err(|e| ApiError::internal(&format!("Failed to list search keys: {}", e)))?;

    let response = entries
        .iter()
        .map(|k| SearchKeyEntryResponse {
            provider: k.provider.clone(),
            key_preview: k.key_preview.clone(),
        })
        .collect();

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
    gw.vault
        .store_search_key(&body.provider, &body.key)
        .map_err(|e| ApiError::internal(&format!("Failed to store search key: {}", e)))?;

    // Rebuild search_list cache so AgentHello picks up the new provider.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw); // Release write lock before hot-push

    // Hot-push search config change to all connected agents
    if let Some(ref pusher) = state.pusher {
        pusher.push_search_config().await;
    }

    Ok((
        StatusCode::CREATED,
        Json(MessageResponse {
            message: format!("Search key stored for provider: {}", body.provider),
        }),
    ))
}

/// `DELETE /api/search/keys/:provider` — remove a search provider API key
pub async fn remove_search_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;
    gw.vault.remove_search_key(&provider).map_err(|e| {
        ApiError::not_found(&format!("Search key not found for '{}': {}", provider, e))
    })?;

    // Rebuild search_list cache after removal.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw);

    if let Some(ref pusher) = state.pusher {
        pusher.push_search_config().await;
    }

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
        _ => match gw.vault.get_search_key(&provider) {
            Ok(entry) => entry.api_key,
            Err(e) => {
                return Err(ApiError::not_found(&format!(
                    "Search key not found for '{}': {}",
                    provider, e
                )));
            }
        },
    };

    // Remove old entry, store new
    let _ = gw.vault.remove_search_key(&provider);
    gw.vault
        .store_search_key(&provider, &api_key)
        .map_err(|e| ApiError::internal(&format!("Failed to update search key: {}", e)))?;

    // Rebuild search_list cache after update.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::rebuild_and_save_search_cache(&mut gw, &data_dir);
    drop(gw);

    if let Some(ref pusher) = state.pusher {
        pusher.push_search_config().await;
    }

    Ok(Json(MessageResponse {
        message: format!("Search key updated for provider: {}", provider),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Merge user-provided model capabilities overrides into a ProviderListItem.
///
/// For each model ID present in `user_caps`:
/// - If the model already exists in `item.models`, user-set fields override
///   the offline data (only non-None user fields are applied).
/// - If the model is not in `item.models`, the override is silently ignored
///   (only configured models can have overrides).
fn merge_user_capabilities(
    item: &mut acowork_core::protocol::ProviderListItem,
    user_caps: &HashMap<String, ModelCapabilitiesInfo>,
) {
    for model_entry in &mut item.models {
        if let Some(user_cap) = user_caps.get(&model_entry.id) {
            // Core limits — always override (frontend always sends these)
            model_entry.capabilities.context_window = user_cap.context_window;
            model_entry.capabilities.max_output_tokens = user_cap.max_output_tokens;
            model_entry.capabilities.supports_tool_calling = user_cap.supports_tool_calling;

            // Optional fields — override only when the user explicitly set them
            if user_cap.modalities.is_some() {
                model_entry.capabilities.modalities = user_cap.modalities.clone();
            }
            if user_cap.supports_attachment.is_some() {
                model_entry.capabilities.supports_attachment = user_cap.supports_attachment;
            }
            if user_cap.default_reasoning_effort.is_some() {
                model_entry.capabilities.default_reasoning_effort =
                    user_cap.default_reasoning_effort.clone();
            }
            if user_cap.thinking_mode.is_some() {
                model_entry.capabilities.thinking_mode = user_cap.thinking_mode.clone();
            }
            if user_cap.supports_reasoning.is_some() {
                model_entry.capabilities.supports_reasoning = user_cap.supports_reasoning;
            }
            if user_cap.supports_temperature.is_some() {
                model_entry.capabilities.supports_temperature = user_cap.supports_temperature;
            }
        }
    }
}

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
    fn test_add_provider_request_deserialization() {
        let json = r#"{"provider": "openai", "key": "sk-12345"}"#;
        let req: AddProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "openai");
        assert_eq!(req.key, "sk-12345");
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }

    #[test]
    fn test_add_provider_request_with_full_config() {
        let json = r#"{"provider": "deepseek", "key": "sk-abc", "base_url": "https://api.deepseek.com/v1", "default_model": "deepseek-chat"}"#;
        let req: AddProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "deepseek");
        assert_eq!(req.key, "sk-abc");
        assert_eq!(
            req.base_url,
            Some("https://api.deepseek.com/v1".to_string())
        );
        assert_eq!(req.default_model, Some("deepseek-chat".to_string()));
    }

    #[test]
    fn test_update_provider_request_deserialization() {
        let json = r#"{"key": "sk-new-key"}"#;
        let req: UpdateProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new-key".to_string()));
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }

    #[test]
    fn test_update_provider_request_with_full_config() {
        let json = r#"{"key": "sk-new", "base_url": "https://api.custom.com/v1", "default_model": "custom-model"}"#;
        let req: UpdateProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new".to_string()));
        assert_eq!(req.base_url, Some("https://api.custom.com/v1".to_string()));
        assert_eq!(req.default_model, Some("custom-model".to_string()));
    }
}
