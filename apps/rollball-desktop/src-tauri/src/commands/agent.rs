//! Agent management commands

use tauri::State;

use crate::gateway_client::{AgentDetailResponse, AgentListEntry, GenericMessageResponse};
use crate::state::AppState;

/// List all installed agents
#[tauri::command]
pub async fn list_agents(state: State<'_, AppState>) -> Result<Vec<AgentListEntry>, String> {
    let client = state.gateway.read().await;
    client.list_agents().await.map_err(|e| e.to_string())
}

/// Get agent detail
#[tauri::command]
pub async fn get_agent_detail(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<AgentDetailResponse, String> {
    let client = state.gateway.read().await;
    client.get_agent_detail(&agent_id).await.map_err(|e| e.to_string())
}

/// Install an agent from a .agent package
///
/// Reads the package file locally (Desktop App side) and uploads its contents
/// to the Gateway via multipart/form-data. This works across platform boundaries
/// (e.g. Windows client → WSL Gateway) because the file content is transmitted
/// over HTTP rather than relying on shared filesystem paths.
#[tauri::command]
pub async fn install_agent(
    state: State<'_, AppState>,
    package_path: String,
    dev_mode: Option<bool>,
) -> Result<GenericMessageResponse, String> {
    // Read the .agent file into memory on the Desktop App side
    let package_bytes = std::fs::read(&package_path)
        .map_err(|e| format!("Failed to read package file '{}': {}", package_path, e))?;

    if package_bytes.is_empty() {
        return Err("Package file is empty".to_string());
    }

    // Upload bytes to Gateway via multipart
    let client = state.gateway.read().await;
    client
        .install_agent(&package_bytes, dev_mode.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())
}

/// Uninstall an agent
#[tauri::command]
pub async fn uninstall_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client.uninstall_agent(&agent_id).await.map_err(|e| e.to_string())
}

/// Start an agent
#[tauri::command]
pub async fn start_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client.start_agent(&agent_id).await.map_err(|e| e.to_string())
}

/// Stop an agent
#[tauri::command]
pub async fn stop_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client.stop_agent(&agent_id).await.map_err(|e| e.to_string())
}
