//! Agent management HTTP API handlers
//!
//! Implements the Agent CRUD and lifecycle endpoints:
//! - GET    /api/agents           — list all agents with status
//! - GET    /api/agents/:id       — get agent detail
//! - POST   /api/agents/install  — install a .agent package
//! - POST   /api/agents/:id/clone — clone an agent (skeleton or full)
//! - DELETE /api/agents/:id       — uninstall an agent
//! - POST   /api/agents/:id/start — start an agent
//! - POST   /api/agents/:id/stop  — stop a running agent

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::http::agent_config::{self, AgentConfigOverride, AgentConfigResponse, UpdateAgentConfigRequest};
use crate::http::routes::{ApiError, AppState};
use rollball_core::protocol::GatewayResponse;

/// Build the agent management router
pub fn agent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}", get(get_agent_detail).delete(uninstall_agent))
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/{id}/clone", post(clone_agent))
        .route("/api/agents/{id}/start", post(start_agent))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route("/api/agents/{id}/model", get(get_agent_model))
        .route("/api/agents/{id}/config", get(get_agent_config).put(update_agent_config))
}

// ── Response types ────────────────────────────────────────────────────

/// Agent list entry
#[derive(Serialize)]
pub struct AgentListResponse {
    pub agent_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    pub version: String,
    pub running: bool,
    pub connected: bool,
    /// Whether the agent is running in developer mode (Debug Protocol enabled)
    pub dev_mode: bool,
    /// Debug WebSocket port (set when dev_mode is true and agent is running)
    pub debug_port: Option<u16>,
}

/// Agent detail response
#[derive(Serialize)]
pub struct AgentDetailResponse {
    pub agent_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    pub running: bool,
    pub connected: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    /// Debug WebSocket port (set when dev_mode is true and agent is running)
    pub debug_port: Option<u16>,
}

/// Install request (kept for reference; actual install uses Multipart)
#[derive(Deserialize)]
pub struct InstallRequest {
    /// Path to the .agent package file (legacy path-based install)
    pub package_path: String,
    /// Skip signature verification (dev mode)
    #[serde(default)]
    pub dev_mode: bool,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// Agent model info response
#[derive(Serialize)]
pub struct AgentModelResponse {
    /// Provider name (e.g. "minimax", "openai")
    pub provider: String,
    /// Currently active model for this agent
    pub model: String,
    /// All available models for this provider
    pub available_models: Vec<String>,
}

// ── Process liveness check ────────────────────────────────────────────

/// Check if a process with the given PID is still alive.
///
/// Uses `/proc/{pid}` on Linux (always available, no I/O cost since procfs
/// is in-memory). On non-Linux platforms, always returns `true` as a fallback.
///
/// Note: There is an inherent TOCTOU race — the process may exit between
/// this check and when the result is used. This is acceptable because the
/// consequence is only a stale `running: true` that self-corrects on the
/// next API call.
#[cfg(target_os = "linux")]
fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(target_os = "linux"))]
fn is_process_alive(_pid: u32) -> bool {
    true // fallback: assume alive if we have a PID record
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents` — list all installed agents
pub async fn list_agents(
    State(state): State<AppState>,
) -> Json<Vec<AgentListResponse>> {
    let gw = state.gateway_state.read().await;
    let agents: Vec<AgentListResponse> = gw
        .installed_agents
        .values()
        .map(|info| {
            // Verify the process is actually alive (not just in running_agents)
            let running_info = gw.running_agents.get(&info.agent_id);
            let actually_running = running_info
                .map(|r| is_process_alive(r.pid))
                .unwrap_or(false);
            let connected = running_info.map(|r| r.connected).unwrap_or(false);
            AgentListResponse {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                display_name: info.manifest.display_name.clone(),
                role: info.manifest.role.clone(),
                avatar: info.manifest.avatar.clone(),
                version: info.version.clone(),
                running: actually_running,
                connected,
                dev_mode: running_info.map(|r| r.dev_mode).unwrap_or(false),
                debug_port: running_info.and_then(|r| r.debug_port),
            }
        })
        .collect();
    Json(agents)
}

/// `GET /api/agents/:id` — get agent detail
pub async fn get_agent_detail(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDetailResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let info = gw.installed_agents.get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let running_info = gw.running_agents.get(&agent_id);
    // Verify the process is actually alive
    let actually_running = running_info.as_ref()
        .map(|r| is_process_alive(r.pid))
        .unwrap_or(false);
    let connected = running_info.map(|r| r.connected).unwrap_or(false);
    let resp = AgentDetailResponse {
        agent_id: info.agent_id.clone(),
        name: info.name.clone(),
        display_name: info.manifest.display_name.clone(),
        role: info.manifest.role.clone(),
        avatar: info.manifest.avatar.clone(),
        version: info.version.clone(),
        description: info.manifest.description.clone(),
        author: info.manifest.author.clone(),
        install_path: info.install_path.clone(),
        running: actually_running,
        connected,
        pid: running_info.map(|r| r.pid),
        started_at: running_info.map(|r| r.started_at.to_rfc3339()),
        debug_port: running_info.and_then(|r| r.debug_port),
    };
    Ok(Json(resp))
}

/// `POST /api/agents/install` — install a .agent package
///
/// Accepts `multipart/form-data` with:
/// - `package`: the .agent ZIP file bytes (required)
/// - `dev_mode`: "true" or "false" (optional, defaults to Gateway config)
///
/// The uploaded bytes are spooled to a temp file so that [`install_package`]
/// (which takes a `&Path`) can operate on it. The temp file is cleaned up
/// after installation completes — whether success or failure.
pub async fn install_agent(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    // Parse multipart fields
    let mut package_bytes: Option<Vec<u8>> = None;
    let mut request_dev_mode: Option<bool> = None;

    while let Some(field) = multipart.next_field().await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await
                    .map_err(|e| ApiError::bad_request(&format!("Failed to read package field: {}", e)))?;
                package_bytes = Some(bytes.to_vec());
            }
            "dev_mode" => {
                let text = field.text().await.unwrap_or_default();
                request_dev_mode = Some(text == "true" || text == "1");
            }
            _ => {} // ignore unknown fields
        }
    }

    let package_bytes = package_bytes
        .ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;

    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    // Determine packages dir and dev_mode from Gateway config
    let packages_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
    };

    let dev_mode = match request_dev_mode {
        Some(v) => v,
        None => {
            let gw = state.gateway_state.read().await;
            gw.config.as_ref().map(|c| c.dev_mode).unwrap_or(false)
        }
    };

    // Spool uploaded bytes to a temp file, then call install_package(&Path)
    let install_result = tokio::task::spawn_blocking(move || {
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join(format!(
            "rollball-install-{}-{}.agent",
            std::process::id(),
            timestamp_nanos(),
        ));

        // Write bytes to temp file (ensures install_package always has a real path)
        if let Err(e) = std::fs::write(&temp_file, &package_bytes) {
            return Err(GatewayError::Package(format!(
                "Failed to write upload to temp file: {}", e
            )));
        }

        // Perform install using the original path-based API
        let result = crate::package_manager::install::install_package(
            &temp_file,
            &packages_dir,
            &mut state.gateway_state.blocking_write(),
            dev_mode,
        );

        // Best-effort cleanup of temp file (log but don't fail on cleanup error)
        let _ = std::fs::remove_file(&temp_file);

        result
    }).await;

    /// Simple nanosecond timestamp for unique temp filenames
    fn timestamp_nanos() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    match install_result {
        Ok(Ok(info)) => Ok((StatusCode::CREATED, Json(MessageResponse {
            message: format!("Package installed: {}", info.agent_id),
        }))),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Install failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Install task failed: {}", e))),
    }
}

/// Clone mode: skeleton or full
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneModeParam {
    Skeleton,
    Full,
}

/// Clone request body
#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    /// New agent ID for the cloned agent
    pub new_agent_id: String,
    /// Clone mode: "skeleton" or "full"
    #[serde(default = "default_clone_mode")]
    pub mode: CloneModeParam,
}

fn default_clone_mode() -> CloneModeParam {
    CloneModeParam::Skeleton
}

/// Clone response
#[derive(Debug, Serialize)]
pub struct CloneResponse {
    pub agent_id: String,
    pub install_path: String,
}

/// `POST /api/agents/:id/clone` — clone an agent
pub async fn clone_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<CloneRequest>,
) -> Result<(StatusCode, Json<CloneResponse>), (StatusCode, Json<ApiError>)> {
    // Validate new_agent_id is different from source
    if req.new_agent_id == agent_id {
        return Err(ApiError::bad_request(
            "new_agent_id must be different from source agent_id",
        ));
    }

    // Determine packages dir and dev_mode from Gateway config
    let packages_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
    };

    let new_agent_id = req.new_agent_id.clone();

    let result = tokio::task::spawn_blocking(move || {
        let mut gw = state.gateway_state.blocking_write();
        let clone_mode = match req.mode {
            CloneModeParam::Skeleton => {
                crate::package_manager::clone::CloneMode::Skeleton
            }
            CloneModeParam::Full => crate::package_manager::clone::CloneMode::Full,
        };

        crate::package_manager::clone::clone_agent(
            &agent_id,
            &new_agent_id,
            clone_mode,
            &packages_dir,
            &mut gw,
        )
    })
    .await;

    match result {
        Ok(Ok(info)) => Ok((
            StatusCode::CREATED,
            Json(CloneResponse {
                agent_id: info.agent_id,
                install_path: info.install_path,
            }),
        )),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Clone failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Clone task failed: {}", e))),
    }
}

/// `DELETE /api/agents/:id` — uninstall an agent
///
/// P1-9 fix: Uses spawn_blocking because uninstall_package performs
/// synchronous database operations (CronStore delete_by_agent) that
/// would block the tokio runtime if called directly in an async handler.
pub async fn uninstall_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    // Check if agent is running first (lightweight read)
    {
        let gw = state.gateway_state.read().await;
        if gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is running, stop it first", agent_id
            )));
        }
    }

    // Determine packages dir from Gateway config
    let packages_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
    };

    // Wrap the synchronous uninstall in spawn_blocking
    let agent_id_display = agent_id.clone();
    let uninstall_result = tokio::task::spawn_blocking(move || {
        let mut gw = state.gateway_state.blocking_write();
        crate::package_manager::uninstall::uninstall_package(
            &agent_id,
            &packages_dir,
            &mut gw,
        )
    }).await;

    match uninstall_result {
        Ok(Ok(_)) => Ok(Json(MessageResponse {
            message: format!("Agent uninstalled: {}", agent_id_display),
        })),
        Ok(Err(e)) => Err(ApiError::internal(&format!("Uninstall failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Uninstall task failed: {}", e))),
    }
}

/// Start agent request body
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct StartAgentRequest {
    /// Start in developer mode (enables Debug Protocol WebSocket)
    pub dev_mode: bool,
}

impl Default for StartAgentRequest {
    fn default() -> Self {
        Self { dev_mode: false }
    }
}

/// `POST /api/agents/:id/start` — start an agent
pub async fn start_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<StartAgentRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_installed(&agent_id) {
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
    }
    if gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!("Agent {} is already running", agent_id)));
    }

    // Use the lifecycle manager to start the agent
    let idle_timeout = 300; // Default idle timeout
    let grpc_addr = crate::grpc::server::default_grpc_addr();
    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, gateway_grpc_endpoint);
    lifecycle.start_agent(&agent_id, &mut gw, req.dev_mode).await
        .map_err(|e| ApiError::internal(&format!("Start failed: {}", e)))?;
    drop(gw);

    // When starting in debug mode, bump Gateway's log level to DEBUG
    // so the Settings UI reflects the effective log level.
    if req.dev_mode {
        let level = "debug";
        // 1. Update stored config
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(config) = &mut gw.config {
                config.log_level = level.to_string();
            }
        }
        // 2. Apply to Gateway's own tracing subscriber
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = tracing_subscriber::EnvFilter::new(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!("Failed to reload Gateway tracing filter for debug mode: {}", e);
            } else {
                tracing::info!("Gateway log level set to {} (debug mode agent start)", level);
            }
        }
    }

    let mode_label = if req.dev_mode { " (dev mode)" } else { "" };
    Ok(Json(MessageResponse {
        message: format!("Agent started: {}{}", agent_id, mode_label),
    }))
}

/// `POST /api/agents/:id/stop` — stop a running agent
pub async fn stop_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!("Agent {} is not running", agent_id)));
    }

    let idle_timeout = 300;
    let grpc_addr = crate::grpc::server::default_grpc_addr();
    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, gateway_grpc_endpoint);
    lifecycle.stop_agent(&agent_id, &mut gw).await
        .map_err(|e| ApiError::internal(&format!("Stop failed: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Agent stopped: {}", agent_id),
    }))
}

/// `GET /api/agents/:id/model` — get the current active model for an agent
///
/// Reads the per-agent model preference from the workspace `.agent_model.json` file.
/// If no per-agent preference exists, falls back to the Gateway config default_model,
/// then the Vault entry's default_model (models[0]).
pub async fn get_agent_model(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentModelResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    let info = gw.installed_agents.get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    // Resolve provider and models from Gateway config / Vault
    let default_provider = gw.config.as_ref()
        .and_then(|c| c.default_provider.as_deref())
        .map(|s| s.to_string())
        .or_else(|| gw.vault.list_providers().first().cloned());

    let provider_name = match default_provider {
        Some(name) => name,
        None => return Err(ApiError::not_found("No provider configured in Vault")),
    };

    let vault_entry = gw.vault.get_provider(&provider_name)
        .map_err(|e| ApiError::internal(&format!("Vault error: {}", e)))?;

    let config_default_model = gw.config.as_ref()
        .and_then(|c| c.default_model.as_deref());

    // Gateway-level default model (config > Vault default_model)
    let gateway_model = config_default_model
        .map(|m| m.to_string())
        .or(vault_entry.default_model.clone())
        .unwrap_or_default();

    // Try reading per-agent model preference from workspace
    let workspace = std::path::Path::new(&info.install_path).join("workspace");
    let model_path = workspace.join(".agent_model.json");
    let (active_model, active_provider) = if model_path.exists() {
        match std::fs::read_to_string(&model_path) {
            Ok(content) => {
                // Parse both "model" and "provider" fields
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&content) {
                    let model = obj.get("model")
                        .and_then(|v| v.as_str())
                        .map(|m| m.to_string());
                    let provider = obj.get("provider")
                        .and_then(|v| v.as_str())
                        .map(|p| p.to_string());
                    (model, provider)
                } else {
                    (None, None)
                }
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    // Resolve vault entry: prefer per-agent provider, fallback to default
    let (resolved_vault_entry, resolved_provider_name) = if let Some(ref ap) = active_provider {
        if ap != &provider_name {
            // Per-agent model is from a different provider; look up that provider's vault entry
            match gw.vault.get_provider(ap) {
                Ok(entry) => (entry, ap.clone()),
                Err(_) => {
                    // Per-agent provider no longer exists in vault, fall back to default
                    (vault_entry, provider_name.clone())
                }
            }
        } else {
            (vault_entry, provider_name.clone())
        }
    } else {
        (vault_entry, provider_name.clone())
    };

    // Use per-agent preference if available and valid in the CORRECT provider's models
    let resolved_model = match active_model {
        Some(ref m) if resolved_vault_entry.models.contains(m) => m.clone(),
        _ => gateway_model,
    };

    let resolved_provider = active_provider.unwrap_or(resolved_provider_name);

    Ok(Json(AgentModelResponse {
        provider: resolved_provider,
        model: resolved_model,
        available_models: resolved_vault_entry.models,
    }))
}

// ── Agent config handlers ─────────────────────────────────────────────

/// Read the system prompt from the agent's prompts directory.
/// Concatenates all .md and .txt files sorted by filename.
fn read_system_prompt(install_path: &str) -> Option<String> {
    let prompts_dir = std::path::Path::new(install_path).join("prompts");
    if !prompts_dir.exists() {
        return None;
    }
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(&prompts_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map_or(false, |ext| ext == "md" || ext == "txt")
            })
            .collect(),
        Err(_) => return None,
    };
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut prompt = String::new();
    for file in &files {
        match std::fs::read_to_string(file) {
            Ok(content) => {
                if !prompt.is_empty() {
                    prompt.push('\n');
                }
                prompt.push_str(&content);
            }
            Err(_) => continue,
        }
    }
    if prompt.is_empty() {
        None
    } else {
        Some(prompt)
    }
}

/// `GET /api/agents/{id}/config` — get agent runtime config
///
/// Returns the effective config for an agent by merging per-agent overrides
/// with global Gateway defaults.
pub async fn get_agent_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentConfigResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    let info = gw
        .installed_agents
        .get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    // Get data_dir from Gateway config
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));

    // Get global max_output_tokens from config
    let global_max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS);

    // Load per-agent config override
    let per_agent = agent_config::load_agent_config(&data_dir, &agent_id).unwrap_or(None);

    // Read system prompt from install_path/prompts/
    let system_prompt = read_system_prompt(&info.install_path);

    // Merge into effective config
    let effective = agent_config::get_effective_config(
        &agent_id,
        per_agent.as_ref(),
        global_max_output_tokens,
        system_prompt,
    );

    Ok(Json(effective))
}

/// `PUT /api/agents/{id}/config` — update agent runtime config
///
/// Accepts partial updates: only provided fields are modified.
/// Saves the updated config to disk and pushes RuntimeConfigUpdate
/// to the connected agent if available.
pub async fn update_agent_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAgentConfigRequest>,
) -> Result<Json<AgentConfigResponse>, (StatusCode, Json<ApiError>)> {
    // Extract data from gateway state first (release lock before async ops)
    let (info_install_path, data_dir, global_max_output_tokens) = {
        let gw = state.gateway_state.read().await;

        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

        let data_dir = gw
            .config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"));

        let global_max_output_tokens = gw
            .config
            .as_ref()
            .map(|c| c.max_output_tokens_limit)
            .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS);

        (info.install_path.clone(), data_dir, global_max_output_tokens)
    };

    // Load existing config or create default
    let existing = agent_config::load_agent_config(&data_dir, &agent_id)
        .unwrap_or(None)
        .unwrap_or_default();

    // Merge update: provided values override, None means keep existing
    // Clone String fields before move so we can use them for push later
    let req_system_prompt_override = req.system_prompt_override.clone();
    let updated = AgentConfigOverride {
        max_output_tokens: req.max_output_tokens.or(existing.max_output_tokens),
        max_iterations: req.max_iterations.or(existing.max_iterations),
        temperature: req.temperature.or(existing.temperature),
        system_prompt_override: req
            .system_prompt_override
            .or(existing.system_prompt_override),
    };

    // Save to disk
    agent_config::save_agent_config(&data_dir, &agent_id, &updated)
        .map_err(|e| ApiError::internal(&format!("Failed to save config: {}", e)))?;

    // Push RuntimeConfigUpdate to connected agent if available
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    max_output_tokens: req.max_output_tokens,
                    max_iterations: req.max_iterations,
                    temperature: req.temperature,
                    system_prompt_override: req_system_prompt_override,
                })
                .await;
            if !push_result {
                tracing::warn!(
                    agent_id = %agent_id,
                    "Failed to push RuntimeConfigUpdate to connected agent"
                );
            }
        }
    }

    // Read system prompt for response
    let system_prompt = read_system_prompt(&info_install_path);

    // Return updated effective config
    let effective = agent_config::get_effective_config(
        &agent_id,
        Some(&updated),
        global_max_output_tokens,
        system_prompt,
    );

    Ok(Json(effective))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_list_response_serialization() {
        let resp = AgentListResponse {
            agent_id: "com.example.weather".to_string(),
            name: "Weather Agent".to_string(),
            display_name: None,
            role: None,
            avatar: None,
            version: "1.0.0".to_string(),
            running: false,
            connected: false,
            dev_mode: false,
            debug_port: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("com.example.weather"));
        assert!(json.contains("Weather Agent"));
    }

    #[test]
    fn test_install_request_deserialization() {
        let json = r#"{"package_path": "/tmp/weather.agent"}"#;
        let req: InstallRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.package_path, "/tmp/weather.agent");
    }

    #[test]
    fn test_message_response_serialization() {
        let resp = MessageResponse {
            message: "Agent started".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Agent started"));
    }
}
