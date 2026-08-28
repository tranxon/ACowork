//! Agent management commands

use tauri::{Manager, State};

use crate::gateway_client::{
    AgentDetailResponse, AgentListEntry, CloneResponse, GatewayApiError, GenericMessageResponse,
    OperationAck,
};
use crate::state::{AppState, BootstrapStateView};

/// Maximum number of attempts for an `install_agent` POST to the Gateway
/// before giving up. The Gateway returns HTTP 409 with the structured
/// `dependency_not_ready` code while the Node control plane is still
/// bootstrapping (ADR-059 §6.3). Most retries recover within 1–2
/// iterations; we cap at 5 to keep the user-perceived onboarding
/// latency bounded.
const INSTALL_MAX_ATTEMPTS: usize = 5;

/// Time budget for waiting on the bootstrap phase to reach READY
/// between install retries (ADR-059 §5.1).
const INSTALL_BOOTSTRAP_WAIT_SECS: u64 = 30;

/// Decide whether an `install_agent` failure is retryable.
///
/// ADR-059 §6.3: only the structured `dependency_not_ready` code is
/// retried — the node control plane has not announced readiness yet, so
/// we wait for bootstrap phase READY and resubmit. The old
/// "503 / never enrolled" text matching is gone (Phase 5.5).
fn should_retry_install(err: &anyhow::Error) -> bool {
    err.downcast_ref::<GatewayApiError>()
        .map(|e| e.is_dependency_not_ready())
        .unwrap_or(false)
}

/// Poll `GET /api/bootstrap` until the aggregated phase is READY
/// (ADR-059 §5.1). The Gateway folds every required subsystem (vault /
/// mqtt / node.local / system_agent / publisher) into the phase, so a
/// single wait covers the whole dependency set — no per-subsystem
/// guessing. Returns the phase detail on terminal phases.
async fn wait_bootstrap_ready(
    state: &AppState,
    timeout_secs: u64,
) -> Result<BootstrapStateView, String> {
    let base_url = state.gateway.read().await.base_url().to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut last_detail = String::new();
    while std::time::Instant::now() < deadline {
        match client
            .get(format!("{}/api/bootstrap", base_url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(view) = resp.json::<BootstrapStateView>().await {
                    match view.phase.as_str() {
                        "READY" => return Ok(view),
                        "FAILED" | "SHUTTING_DOWN" => {
                            return Err(format!(
                                "Gateway {}: {}",
                                view.phase, view.phase_detail
                            ));
                        }
                        _ => last_detail = view.phase_detail.clone(),
                    }
                }
            }
            Ok(_) | Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    Err(format!(
        "bootstrap did not reach READY within {}s ({})",
        timeout_secs, last_detail
    ))
}

/// Shared retry wrapper for the Gateway install POST. Returns the first
/// non-retryable error or the final result after `INSTALL_MAX_ATTEMPTS`
/// attempts. On `dependency_not_ready` the retry waits for bootstrap
/// phase READY first (ADR-059 §6.3) instead of a blind sleep.
async fn install_with_retry<F, Fut>(
    state: &AppState,
    label: &str,
    mut op: F,
) -> Result<OperationAck, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<OperationAck>>,
{
    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(ack) => {
                if attempt > 1 {
                    tracing::info!(
                        "[{}] Install accepted on attempt {}/{}",
                        label,
                        attempt,
                        INSTALL_MAX_ATTEMPTS
                    );
                }
                return Ok(ack);
            }
            Err(e) => {
                let retryable = should_retry_install(&e);
                if !retryable || attempt >= INSTALL_MAX_ATTEMPTS {
                    return Err(e.to_string());
                }
                tracing::warn!(
                    "[{}] Install attempt {}/{} rejected (dependency_not_ready, \
                     waiting for bootstrap READY): {}",
                    label,
                    attempt,
                    INSTALL_MAX_ATTEMPTS,
                    e
                );
                wait_bootstrap_ready(state, INSTALL_BOOTSTRAP_WAIT_SECS).await?;
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
/// ADR-059 §6: the Gateway answers HTTP 202 with an [`OperationAck`]; the
/// actual install runs asynchronously on the node (completion is correlated
/// via the ack's `operation_id`). While the node control plane is still
/// bootstrapping it answers HTTP 409 `dependency_not_ready` instead — we
/// wait for bootstrap phase READY and resubmit up to `INSTALL_MAX_ATTEMPTS`
/// times.
#[tauri::command]
pub async fn install_agent(
    state: State<'_, AppState>,
    package_path: String,
    dev_mode: Option<bool>,
    node_id: Option<String>,
) -> Result<OperationAck, String> {
    // Read the .agent file into memory on the Desktop App side
    let package_bytes = std::fs::read(&package_path)
        .map_err(|e| format!("Failed to read package file '{}': {}", package_path, e))?;

    if package_bytes.is_empty() {
        return Err("Package file is empty".to_string());
    }

    // Clone the captured Tauri `State` outside the closure so the retry
    // wrapper can borrow `state` while the `move` closure owns its copy.
    let state_for_op = state.clone();
    install_with_retry(&state, "INSTALL_AGENT", move || {
        let state = state_for_op.clone();
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
) -> Result<OperationAck, String> {
    if resource_name.contains('/') || resource_name.contains('\\') || resource_name.contains("..") {
        return Err("Invalid bundled agent name".to_string());
    }

    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let package_file = bundled_agent_package_path(&resource_dir, &resource_name);
    tracing::info!(
        "[INSTALL_BUNDLED] Reading bundled package for {} from {:?}",
        resource_name,
        package_file
    );
    let package_bytes = std::fs::read(&package_file).map_err(|e| {
        format!(
            "Failed to read bundled agent package '{}': {}",
            package_file.display(),
            e
        )
    })?;
    tracing::info!(
        "[INSTALL_BUNDLED] Read {} bytes for {}, submitting install",
        package_bytes.len(),
        resource_name
    );

    let state_for_op = state.clone();
    install_with_retry(&state, "INSTALL_BUNDLED", move || {
        // See INSTALL_AGENT — the retry wrapper borrows `state` while this
        // closure owns its pre-cloned copy.
        let state = state_for_op.clone();
        let package_bytes = package_bytes.clone();
        let dev_mode = dev_mode.unwrap_or(true);
        async move {
            let client = state.gateway.read().await;
            client.install_agent(&package_bytes, dev_mode, None).await
        }
    })
    .await
}

/// Wait until the agent appears in the Gateway inventory.
///
/// ADR-059 §6: install is asynchronous — `install_bundled_agent`
/// returns the [`OperationAck`] immediately (HTTP 202) and the node
/// installs in the background; the Gateway aggregates the node's
/// retained `InstalledAgentInfo` into the inventory on completion. This
/// command polls `GET /api/agents/{id}` until the agent is visible —
/// the authoritative completion signal (no fixed sleeps, no phase
/// guessing).
#[tauri::command]
pub async fn wait_agent_installed(
    state: State<'_, AppState>,
    agent_id: String,
    timeout_secs: Option<u64>,
) -> Result<AgentDetailResponse, String> {
    let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(120));
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(&agent_id, timeout)
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
