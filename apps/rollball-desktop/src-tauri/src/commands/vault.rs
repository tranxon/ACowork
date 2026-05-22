//! Vault key management commands

use std::collections::HashMap;
use tauri::State;

use crate::gateway_client::{GenericMessageResponse, ModelCapabilities, VaultKeyEntry};
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
