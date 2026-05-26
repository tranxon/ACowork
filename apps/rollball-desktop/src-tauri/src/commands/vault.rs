//! Vault key management commands

use std::collections::HashMap;
use tauri::State;

use crate::gateway_client::{GenericMessageResponse, ModelCapabilities, SearchVaultKeyEntry, VaultKeyEntry};
use crate::state::AppState;

/// List all stored API keys (masked)
#[tauri::command]
pub async fn list_keys(state: State<'_, AppState>) -> Result<Vec<VaultKeyEntry>, String> {
    let client = state.gateway.read().await;
    client.list_keys().await.map_err(|e| e.to_string())
}

/// Add a new API key (with optional base_url, models list, and per-model capabilities)
#[tauri::command]
pub async fn add_key(
    state: State<'_, AppState>,
    provider: String,
    key: String,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
    model_capabilities: Option<HashMap<String, ModelCapabilities>>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    let caps = model_capabilities.unwrap_or_default();
    client
        .add_key(
            &provider,
            &key,
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
            &caps,
        )
        .await
        .map_err(|e| e.to_string())
}

/// Remove an API key
#[tauri::command]
pub async fn remove_key(
    state: State<'_, AppState>,
    provider: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client.remove_key(&provider).await.map_err(|e| e.to_string())
}

/// Update an API key (supports partial updates — key is optional)
#[tauri::command]
pub async fn update_key(
    state: State<'_, AppState>,
    provider: String,
    key: Option<String>,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
    model_capabilities: Option<HashMap<String, ModelCapabilities>>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    let caps = model_capabilities.unwrap_or_default();
    client
        .update_key(
            &provider,
            key.as_deref(),
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
            &caps,
        )
        .await
        .map_err(|e| e.to_string())
}

// ── Search key commands ──────────────────────────────────────────────

/// List all stored search provider API keys (masked)
#[tauri::command]
pub async fn list_search_keys(state: State<'_, AppState>) -> Result<Vec<SearchVaultKeyEntry>, String> {
    let client = state.gateway.read().await;
    client.list_search_keys().await.map_err(|e| e.to_string())
}

/// Add a new search provider API key
#[tauri::command]
pub async fn add_search_key(
    state: State<'_, AppState>,
    provider: String,
    key: String,
    base_url: Option<String>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .add_search_key(&provider, &key, base_url.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Remove a search provider API key
#[tauri::command]
pub async fn remove_search_key(
    state: State<'_, AppState>,
    provider: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client.remove_search_key(&provider).await.map_err(|e| e.to_string())
}

/// Update a search provider API key (supports partial updates)
#[tauri::command]
pub async fn update_search_key(
    state: State<'_, AppState>,
    provider: String,
    key: Option<String>,
    base_url: Option<String>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .update_search_key(&provider, key.as_deref(), base_url.as_deref())
        .await
        .map_err(|e| e.to_string())
}
