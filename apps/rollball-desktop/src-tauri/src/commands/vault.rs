//! Vault key management commands

use tauri::State;

use crate::gateway_client::{GenericMessageResponse, VaultKeyEntry};
use crate::state::AppState;

/// List all stored API keys (masked)
#[tauri::command]
pub async fn list_keys(state: State<'_, AppState>) -> Result<Vec<VaultKeyEntry>, String> {
    let client = state.gateway.read().await;
    client.list_keys().await.map_err(|e| e.to_string())
}

/// Add a new API key (with optional base_url and models list)
#[tauri::command]
pub async fn add_key(
    state: State<'_, AppState>,
    provider: String,
    key: String,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .add_key(
            &provider,
            &key,
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
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

/// Update an API key (with optional base_url and models list)
#[tauri::command]
pub async fn update_key(
    state: State<'_, AppState>,
    provider: String,
    key: String,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .update_key(
            &provider,
            &key,
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())
}
