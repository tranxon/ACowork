//! Settings/config commands

use tauri::State;

use crate::gateway_client::{ConfigResponse, GenericMessageResponse};
use crate::state::AppState;

/// Get Gateway configuration
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<ConfigResponse, String> {
    let client = state.gateway.read().await;
    client.get_config().await.map_err(|e| e.to_string())
}

/// Update Gateway configuration
#[tauri::command]
pub async fn update_config(
    state: State<'_, AppState>,
    log_level: Option<String>,
    idle_timeout_secs: Option<u64>,
    default_provider: Option<String>,
    default_model: Option<String>,
    max_output_tokens_limit: Option<u64>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .update_config(
            log_level.as_deref(),
            idle_timeout_secs,
            default_provider.as_deref(),
            default_model.as_deref(),
            max_output_tokens_limit,
        )
        .await
        .map_err(|e| e.to_string())
}
