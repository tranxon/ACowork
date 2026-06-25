//! Agent management commands

use tauri::{Manager, State};

use crate::gateway_client::{
    AgentDetailResponse, AgentListEntry, CloneResponse, GenericMessageResponse,
};
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
    client
        .get_agent_detail(&agent_id)
        .await
        .map_err(|e| e.to_string())
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

#[tauri::command]
pub async fn install_bundled_agent(
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
    resource_name: String,
    dev_mode: Option<bool>,
) -> Result<GenericMessageResponse, String> {
    if resource_name.contains('/') || resource_name.contains('\\') || resource_name.contains("..") {
        return Err("Invalid bundled agent name".to_string());
    }

    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let package_file = bundled_agent_package_path(&resource_dir, &resource_name);
    let package_bytes = std::fs::read(&package_file).map_err(|e| {
        format!(
            "Failed to read bundled agent package '{}': {}",
            package_file.display(),
            e
        )
    })?;

    let client = state.gateway.read().await;
    client
        .install_agent(&package_bytes, dev_mode.unwrap_or(true))
        .await
        .map_err(|e| e.to_string())
}

fn bundled_agent_package_path(
    resource_dir: &std::path::Path,
    resource_name: &str,
) -> std::path::PathBuf {
    let package_name = match resource_name {
        "system-agent" => "com.acowork.system.agent",
        "software-architect-agent" => "com.acowork.software-architect.agent",
        "senior-engineer-agent" => "com.acowork.senior-engineer.agent",
        "quality-assurance-agent" => "com.acowork.quality-assurance.agent",
        "project-manager-agent" => "com.acowork.project-manager.agent",
        "product-manager-agent" => "com.acowork.product-manager.agent",
        "document-manager-agent" => "com.acowork.document-manager.agent",
        other => {
            return resource_dir
                .join("agent-packages")
                .join(format!("{}.agent", other));
        }
    };
    resource_dir.join("agent-packages").join(package_name)
}

/// Uninstall an agent
#[tauri::command]
pub async fn uninstall_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .uninstall_agent(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Start an agent
#[tauri::command]
pub async fn start_agent(
    state: State<'_, AppState>,
    agent_id: String,
    dev_mode: Option<bool>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .start_agent(&agent_id, dev_mode.unwrap_or(false))
        .await
        .map_err(|e| e.to_string())
}

/// Stop an agent
#[tauri::command]
pub async fn stop_agent(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .stop_agent(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Restart an agent in debug mode (atomic in-Runtime switch, no process restart)
#[tauri::command]
pub async fn restart_agent_in_debug(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .restart_agent_in_debug(&agent_id)
        .await
        .map_err(|e| e.to_string())
}

/// Clone an agent (skeleton or full mode)
#[tauri::command]
pub async fn clone_agent(
    state: State<'_, AppState>,
    agent_id: String,
    new_agent_id: String,
    mode: Option<String>,
) -> Result<CloneResponse, String> {
    let client = state.gateway.read().await;
    client
        .clone_agent(
            &agent_id,
            &new_agent_id,
            &mode.unwrap_or_else(|| "skeleton".to_string()),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Update the avatar / builtin_avatar fields in the agent's installed
/// `manifest.toml`. Used by the Publish wizard to bake the user's avatar
/// selection into the package before build.
///
/// Pass `Some("...")` to set, `Some("")` or omit to leave the field unchanged.
#[tauri::command]
pub async fn update_agent_manifest_avatar(
    state: State<'_, AppState>,
    agent_id: String,
    avatar: Option<String>,
    builtin_avatar: Option<String>,
) -> Result<serde_json::Value, String> {
    let client = state.gateway.read().await;
    client
        .update_agent_manifest_avatar(
            &agent_id,
            avatar.as_deref(),
            builtin_avatar.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Upload a file into the agent's install directory. Used by the Publish
/// wizard to attach a custom avatar image before the manifest is updated to
/// reference it.
///
/// `relative_path` is the destination path inside the install dir
/// (e.g. "assets/avatar.png"). The server restricts accepted extensions to
/// image formats.
#[tauri::command]
pub async fn upload_agent_file(
    state: State<'_, AppState>,
    agent_id: String,
    relative_path: String,
    file_path: String,
) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read file '{}': {}", file_path, e))?;
    let client = state.gateway.read().await;
    client
        .upload_agent_file(&agent_id, &relative_path, &bytes)
        .await
        .map_err(|e| e.to_string())
}

/// Upload a user avatar image file to the Gateway's `{data_dir}/assets/`.
///
/// The Gateway auto-generates the filename (avatar-01.png, avatar-02.png, etc.)
/// and returns the relative path.
#[tauri::command]
pub async fn upload_user_avatar_file(
    state: State<'_, AppState>,
    file_path: String,
) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read file '{}': {}", file_path, e))?;
    let file_name = std::path::Path::new(&file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("avatar.png");
    let client = state.gateway.read().await;
    client
        .upload_user_avatar_file(&bytes, file_name)
        .await
        .map_err(|e| e.to_string())
}
