//! Agent management commands

use tauri::{Manager, State};

use crate::gateway_client::{
    AgentDetailResponse, AgentListEntry, CloneResponse, GenericMessageResponse,
};
use crate::state::AppState;

/// Maximum number of attempts for an `install_agent` POST to the Gateway
/// before giving up. The Gateway returns HTTP 503 "Node 'local' has never
/// enrolled" while the Node Agent is still bootstrapping (Fix 2). Most
/// retries recover within 1–2 iterations; we cap at 5 / 7.5 s to keep
/// the user-perceived onboarding latency bounded.
const INSTALL_MAX_ATTEMPTS: usize = 5;

/// Install-backoff base: 1.5 s per attempt. Total worst-case wait
/// is `(MAX_ATTEMPTS - 1) * 1.5 s = 6 s`.
const INSTALL_RETRY_DELAY_MS: u64 = 1500;

/// Decide whether an `install_agent` failure looks like a node-online
/// race (the common case in onboarding) and is worth retrying. Network
/// errors and HTTP 503s from the Gateway fall in this bucket; package
/// validation errors do not.
fn should_retry_install(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("503")
        || msg.contains("never enrolled")
        || msg.contains("status code")
}

/// Shared retry wrapper for the Gateway install POST. Returns the first
/// non-retryable error or the final result after `INSTALL_MAX_ATTEMPTS`
/// attempts.
async fn install_with_retry<F, Fut>(label: &str, mut op: F) -> Result<GenericMessageResponse, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<GenericMessageResponse>>,
{
    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(resp) => {
                if attempt > 1 {
                    tracing::info!(
                        "[{}] Install succeeded on attempt {}/{}",
                        label,
                        attempt,
                        INSTALL_MAX_ATTEMPTS
                    );
                }
                return Ok(resp);
            }
            Err(e) => {
                let retryable = should_retry_install(&e);
                if !retryable || attempt >= INSTALL_MAX_ATTEMPTS {
                    return Err(e.to_string());
                }
                tracing::warn!(
                    "[{}] Install attempt {}/{} failed (will retry): {}",
                    label,
                    attempt,
                    INSTALL_MAX_ATTEMPTS,
                    e
                );
                tokio::time::sleep(std::time::Duration::from_millis(INSTALL_RETRY_DELAY_MS))
                    .await;
            }
        }
    }
}

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
///
/// Fix 2: the Gateway can answer HTTP 503 "Node 'local' has never enrolled"
/// while the Node Agent is still bootstrapping. We retry that and similar
/// transient errors up to `INSTALL_MAX_ATTEMPTS` times with `1.5s` backoff.
#[tauri::command]
pub async fn install_agent(
    state: State<'_, AppState>,
    package_path: String,
    dev_mode: Option<bool>,
    node_id: Option<String>,
) -> Result<GenericMessageResponse, String> {
    // Read the .agent file into memory on the Desktop App side
    let package_bytes = std::fs::read(&package_path)
        .map_err(|e| format!("Failed to read package file '{}': {}", package_path, e))?;

    if package_bytes.is_empty() {
        return Err("Package file is empty".to_string());
    }

    install_with_retry("INSTALL_AGENT", move || {
        // Clone the captured Tauri `State` (cheap, it wraps an `&AppState`)
        // for each attempt so the FnMut closure can be called multiple times.
        let state = state.clone();
        let package_bytes = package_bytes.clone();
        let node_id = node_id.clone();
        let dev_mode = dev_mode.unwrap_or(false);
        async move {
            let client = state.gateway.read().await;
            client
                .install_agent(&package_bytes, dev_mode, node_id.as_deref())
                .await
        }
    })
    .await
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

    install_with_retry("INSTALL_BUNDLED", move || {
        // See INSTALL_AGENT — clone the captured Tauri `State` per attempt.
        let state = state.clone();
        let package_bytes = package_bytes.clone();
        let dev_mode = dev_mode.unwrap_or(true);
        async move {
            let client = state.gateway.read().await;
            client.install_agent(&package_bytes, dev_mode, None).await
        }
    })
    .await
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
