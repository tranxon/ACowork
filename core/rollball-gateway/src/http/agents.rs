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
use rollball_core::protocol::{AvailableTool, AvailableToolsResponse};
use rollball_core::AgentManifest;

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
        .route("/api/agents/{id}/tools", get(get_agent_tools))
        .route("/api/agents/{id}/mcp-servers", get(get_agent_mcp_servers).put(update_agent_mcp_servers))
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
    /// Whether the agent's SessionTask is initialized and ready to receive messages
    pub ready: bool,
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
    /// Whether the agent's SessionTask is initialized and ready to receive messages
    pub ready: bool,
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
            let ready = running_info.map(|r| r.ready).unwrap_or(false);
            AgentListResponse {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                display_name: info.manifest.display_name.clone(),
                role: info.manifest.role.clone(),
                avatar: info.manifest.avatar.clone(),
                version: info.version.clone(),
                running: actually_running,
                connected,
                ready,
                dev_mode: running_info.map(|r| r.dev_mode).unwrap_or(false),
                debug_port: running_info.and_then(|r| r.debug_port),
            }
        })
        .collect();
    // Diagnostic: if senior-engineer is running, log its ready state
    // to help trace why frontend polls may not see ready=true promptly.
    if let Some(sr) = gw.running_agents.get("com.rollball.senior-engineer") {
        tracing::info!(
            "[DIAG] list_agents: senior-engineer running=true ready={} connected={}",
            sr.ready, sr.connected
        );
    }
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
    let ready = running_info.map(|r| r.ready).unwrap_or(false);
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
        ready,
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
    let log_file_size_mb = gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10);
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, gateway_grpc_endpoint, log_file_size_mb);
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
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(idle_timeout, gateway_grpc_endpoint, 10);
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
    if !gw.installed_agents.contains_key(&agent_id) {
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
    }

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

    // Per-agent model preference is owned by the Agent Runtime
    // (workspace/config/agent_model.json). The Gateway queries it via
    // QueryConfig IPC when the Runtime is connected.
    let (active_model, active_provider) = {
        if let Some(ref grpc_mgr) = state.grpc_session_mgr {
            let query = rollball_core::proto::server_message::Payload::QueryConfig(
                rollball_core::proto::QueryConfig {
                    request_id: uuid::Uuid::new_v4().to_string(),
                },
            );
            match crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await {
                Some(response) => {
                    if let Some(rollball_core::proto::client_message::Payload::ConfigSnapshot(snap)) = response.payload {
                        (snap.model, snap.provider)
                    } else {
                        (None, None)
                    }
                }
                None => (None, None),
            }
        } else {
            (None, None)
        }
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
/// Read the system prompt from the agent's prompts directory.
///
/// **Deprecated (ADR-009)**: Gateway no longer reads agent workspace files.
/// This function is kept for reference but should not be called in production code.
#[allow(dead_code)]
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

/// Read the tool names declared in the agent's manifest.toml.
///
/// **Deprecated (ADR-009)**: Gateway no longer reads agent workspace files.
/// active_tools should come from per-agent config only.
#[allow(dead_code)]
fn read_manifest_tools(install_path: &str) -> Vec<String> {
    let manifest_path = std::path::Path::new(install_path).join("manifest.toml");
    if !manifest_path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&manifest_path) {
        Ok(toml_str) => {
            match AgentManifest::from_toml(&toml_str) {
                Ok(manifest) => manifest.tools.iter().map(|t| t.name.clone()).collect(),
                Err(_) => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    }
}

/// Write updated `[[tools]]` declarations back to manifest.toml.
///
/// **Deprecated (ADR-009)**: Gateway no longer writes to agent workspace files.
/// active_tools persistence is handled by AgentConfigOverride in Gateway's data_dir.
#[allow(dead_code)]
fn write_manifest_tools(install_path: &str, active_tools: &[String]) {
    let manifest_path = std::path::Path::new(install_path).join("manifest.toml");
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to read manifest for tools write-back: {}", e);
            return;
        }
    };

    // Rebuild the manifest: remove all [[tools]] lines, then append new ones
    let mut lines: Vec<String> = Vec::new();
    let mut skip_tools_block = false;
    let mut changed = false;

    for line in content.lines() {
        if line.trim_start().starts_with("[[tools]]") {
            skip_tools_block = true;
            changed = true;
            continue;
        }
        if skip_tools_block {
            // Also skip inline table lines like `[tools.rag]`
            if line.trim_start().starts_with('[') {
                skip_tools_block = false;
                lines.push(line.to_string());
            }
            // else: still in tools block (config sub-keys), skip
            continue;
        }
        lines.push(line.to_string());
    }

    if !changed && active_tools.is_empty() {
        return; // No tools declared, nothing to change
    }

    // Append new [[tools]] entries
    for tool_name in active_tools {
        lines.push(format!("[[tools]]"));
        lines.push(format!("name = \"{}\"", tool_name));
    }

    let new_content = lines.join("\n") + "\n";
    if let Err(e) = std::fs::write(&manifest_path, new_content) {
        tracing::warn!("Failed to write manifest tools: {}", e);
    } else {
        tracing::info!(
            agent_install_path = %install_path,
            tool_count = active_tools.len(),
            "Updated manifest.toml tools section"
        );
    }
}

/// `GET /api/agents/{id}/config` — get agent runtime config
///
/// Queries the connected Runtime via QueryConfig IPC for per-agent config
/// (Phase 5 refactor: per-agent config is now owned by Runtime workspace).
/// Merges with Gateway global defaults for the response.
pub async fn get_agent_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentConfigResponse>, (StatusCode, Json<ApiError>)> {
    let global_max_output_tokens = {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        // Guard: agent must be running and ready.
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(
                    &format!("Agent '{}' is starting up, please wait", agent_id),
                ));
            }
        } else {
            return Err(ApiError::service_unavailable(
                &format!("Agent '{}' is not started", agent_id),
            ));
        }
        gw.config
            .as_ref()
            .map(|c| c.max_output_tokens_limit)
            .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS)
    };

    // Query Runtime workspace config via IPC (QueryConfig → ConfigSnapshot roundtrip).
    let (model, provider, max_output_tokens, max_iterations, temperature,
         system_prompt_override, active_tools, shell_approval_threshold,
         mcp_servers, available_models) =
        if let Some(ref grpc_mgr) = state.grpc_session_mgr {
            let query = rollball_core::proto::server_message::Payload::QueryConfig(
                rollball_core::proto::QueryConfig {
                    request_id: uuid::Uuid::new_v4().to_string(),
                },
            );
            match crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await {
                Some(response) => {
                    if let Some(rollball_core::proto::client_message::Payload::ConfigSnapshot(snap)) = response.payload {
                        (snap.model, snap.provider,
                         snap.max_output_tokens, snap.max_iterations, snap.temperature,
                         snap.system_prompt_override,
                         if snap.active_tools.is_empty() { None } else { Some(snap.active_tools) },
                         snap.shell_approval_threshold,
                         snap.mcp_servers_json,
                         snap.available_models)
                    } else {
                        (None, None, None, None, None, None, None, None, vec![], vec![])
                    }
                }
                None => (None, None, None, None, None, None, None, None, vec![], vec![]),
            }
        } else {
            (None, None, None, None, None, None, None, None, vec![], vec![])
        };

    // Build the effective config from ConfigSnapshot data
    let effective = AgentConfigResponse {
        agent_id,
        max_output_tokens: max_output_tokens,
        max_iterations: max_iterations,
        temperature,
        system_prompt: None,
        // Use model snap fields for active model/provider in response
        model,
        provider,
        active_tools: active_tools.unwrap_or_default(),
        system_prompt_override,
        shell_approval_threshold,
        mcp_servers,
        available_models,
        global_max_output_tokens,
    };

    Ok(Json(effective))
}

/// `PUT /api/agents/{id}/config` — update agent runtime config
///
/// Accepts partial updates. Forwards to Runtime via RuntimeConfigUpdate push.
/// (Phase 5 refactor): Gateway no longer persists per-agent config locally.
/// The Runtime is the authoritative owner and persists to workspace/config/.
pub async fn update_agent_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAgentConfigRequest>,
) -> Result<Json<AgentConfigResponse>, (StatusCode, Json<ApiError>)> {
    let global_max_output_tokens = {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }
        // Guard: agent must be running and ready.
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(
                    &format!("Agent '{}' is starting up, please wait", agent_id),
                ));
            }
        } else {
            return Err(ApiError::service_unavailable(
                &format!("Agent '{}' is not started", agent_id),
            ));
        }
        gw.config
            .as_ref()
            .map(|c| c.max_output_tokens_limit)
            .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS)
    };

    let req_system_prompt_override = req.system_prompt_override.clone();
    let req_active_tools = req.active_tools.clone();
    let req_shell_approval_threshold = req.shell_approval_threshold;
    let req_mcp_servers = req.mcp_servers.clone();

    // Push RuntimeConfigUpdate to connected agent (no Gateway disk save)
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    max_output_tokens: req.max_output_tokens,
                    max_iterations: req.max_iterations,
                    temperature: req.temperature,
                    system_prompt_override: req_system_prompt_override,
                    active_tools: req_active_tools,
                    shell_approval_threshold: req_shell_approval_threshold.map(|t| format!("{:?}", t).to_lowercase()),
                    mcp_servers: req_mcp_servers,
                    model: None,
                    provider: None,
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

    // Return echo of submitted config (the actual persisted values will be
    // available on next GET, which queries the Runtime via ConfigSnapshot).
    let effective = AgentConfigResponse {
        agent_id,
        max_output_tokens: req.max_output_tokens,
        max_iterations: req.max_iterations,
        temperature: req.temperature,
        system_prompt: None,
        system_prompt_override: req.system_prompt_override,
        active_tools: req.active_tools.unwrap_or_default(),
        shell_approval_threshold: req_shell_approval_threshold.map(|t| format!("{:?}", t).to_lowercase()),
        mcp_servers: vec![],
        available_models: vec![],
        model: None,
        provider: None,
        global_max_output_tokens,
    };

    Ok(Json(effective))
}

/// `GET /api/agents/{id}/tools` — list available tools and current activation state.
///
/// Returns all built-in tools with their names, descriptions, and required
/// permissions, plus the currently active tool names.
pub async fn get_agent_tools(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AvailableToolsResponse>, (StatusCode, Json<ApiError>)> {
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

    // Get all available built-in tools with metadata
    let available = builtin_tool_metadata();

    // Determine active tools: config override > manifest
    let per_agent = agent_config::load_agent_config(&data_dir, &agent_id).unwrap_or(None);
    // ADR-009: active_tools from per-agent config only, not manifest.toml
    let raw_active_tools = per_agent
        .as_ref()
        .and_then(|o| o.active_tools.clone())
        .unwrap_or_default();

    // Normalize platform-specific shell names (bash, powershell, pwsh)
    // to the unified "shell" tool name for consistent UI display.
    let active_tools = normalize_shell_tools(&raw_active_tools);

    Ok(Json(AvailableToolsResponse {
        agent_id,
        tools: available,
        active_tools,
    }))
}

/// Static metadata for all built-in tools.
/// Maps tool name to description and required permissions.
fn builtin_tool_metadata() -> Vec<AvailableTool> {
    vec![
        AvailableTool {
            name: "memory_recall".into(),
            description: "Recall information from the agent's persistent memory".into(),
            required_permissions: vec!["memory:read".into()],
        always_on: false,
        },
        AvailableTool {
            name: "memory_store".into(),
            description: "Store information into the agent's persistent memory".into(),
            required_permissions: vec!["memory:write".into()],
        always_on: false,
        },
        AvailableTool {
            name: "http_request".into(),
            description: "Make HTTP requests to external APIs".into(),
            required_permissions: vec!["network:<url>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "web_fetch".into(),
            description: "Fetch and extract content from web pages".into(),
            required_permissions: vec!["network:<url>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "web_search".into(),
            description: "Search the web for information".into(),
            required_permissions: vec!["search:web".into()],
        always_on: false,
        },
        AvailableTool {
            name: "shell".into(),
            description: "Execute shell commands in the platform's native shell".into(),
            required_permissions: vec!["filesystem:exec".into()],
        always_on: false,
        },
        AvailableTool {
            name: "file_read".into(),
            description: "Read file contents from the filesystem".into(),
            required_permissions: vec!["filesystem:read:<path>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "file_write".into(),
            description: "Write content to files on the filesystem".into(),
            required_permissions: vec!["filesystem:write:<path>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "file_edit".into(),
            description: "Edit existing files with search-and-replace".into(),
            required_permissions: vec!["filesystem:write:<path>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "doc_reader".into(),
            description: "Read and extract text from documents (PDF, DOCX, PPTX, XLSX)".into(),
            required_permissions: vec!["filesystem:read:<path>".into()],
            always_on: false,
        },
        AvailableTool {
            name: "glob_search".into(),
            description: "Search for files matching glob patterns".into(),
            required_permissions: vec!["filesystem:read:<path>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "content_search".into(),
            description: "Search file contents with regex/grep".into(),
            required_permissions: vec!["filesystem:read:<path>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "intent_send".into(),
            description: "Send an intent message to another agent".into(),
            required_permissions: vec!["intent:send:<target>".into()],
        always_on: false,
        },
        AvailableTool {
            name: "rag_query".into(),
            description: "Query enterprise RAG knowledge base".into(),
            required_permissions: vec!["rag:query".into(), "network:<rag_url>".into()],
            always_on: false,
        },
        AvailableTool {
            name: "ask_user_question".into(),
            description: "Present a question with options for the user to answer".into(),
            required_permissions: vec![],
            always_on: true,
        },
    ]
}

/// Normalize platform-specific shell tool names in the active tools list.
///
/// The Runtime registers platform-specific shells (e.g. "bash", "powershell"
/// on Windows; "shell" on Linux/macOS), but the agent setup UI presents a
/// single unified "shell" toggle.  This function maps any platform-specific
/// shell variants back to "shell" so the UI checkbox reflects the correct
/// state, and deduplicates in case multiple variants were present.
fn normalize_shell_tools(tools: &[String]) -> Vec<String> {
    let shell_names: &[&str] = &["bash", "powershell", "pwsh"];
    let has_shell_variant = tools.iter().any(|t| shell_names.contains(&t.as_str()));
    if !has_shell_variant {
        return tools.to_vec();
    }
    let mut result: Vec<String> = tools
        .iter()
        .filter(|t| !shell_names.contains(&t.as_str()))
        .cloned()
        .collect();
    // Deduplicate: only push "shell" once
    if !result.contains(&"shell".to_string()) {
        result.push("shell".to_string());
    }
    result
}

// ── Agent MCP server activation handlers ─────────────────────────────

/// MCP server activation response (per-agent)
#[derive(Serialize)]
pub struct AgentMcpServersResponse {
    pub agent_id: String,
    /// Names of active MCP servers (resolved from catalog)
    pub active_servers: Vec<String>,
}

/// Request body for PUT /api/agents/{id}/mcp-servers
#[derive(Deserialize)]
pub struct UpdateMcpServersRequest {
    /// List of MCP server names to activate (from catalog)
    pub servers: Vec<String>,
}

/// `GET /api/agents/{id}/mcp-servers` — get active MCP server names for an agent
///
/// Returns the list of MCP server names that are currently active for this agent.
/// These names reference entries in the global MCP catalog.
pub async fn get_agent_mcp_servers(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentMcpServersResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    if !gw.installed_agents.contains_key(&agent_id) {
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
    }

    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));

    // Load per-agent config
    let per_agent = agent_config::load_agent_config(&data_dir, &agent_id).unwrap_or(None);

    // Extract MCP server names from the per-agent config
    // Current model: mcp_servers stores full McpServerConfigDef — extract names
    let active_servers = per_agent
        .and_then(|cfg| cfg.mcp_servers)
        .map(|servers| servers.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();

    Ok(Json(AgentMcpServersResponse {
        agent_id,
        active_servers,
    }))
}

/// `PUT /api/agents/{id}/mcp-servers` — set active MCP servers for an agent
///
/// Accepts a list of MCP server names. The Gateway:
/// 1. Looks up each name in the global MCP catalog to get full config
/// 2. Merges catalog definitions with any per-agent overrides
/// 3. Saves the full configs to per-agent config
/// 4. Pushes RuntimeConfigUpdate to the running agent via IPC
pub async fn update_agent_mcp_servers(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateMcpServersRequest>,
) -> Result<Json<AgentMcpServersResponse>, (StatusCode, Json<ApiError>)> {
    // Extract data from gateway state
    let data_dir = {
        let gw = state.gateway_state.read().await;

        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
        }

        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    };

    // Load catalog
    let catalog = crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir)
        .map_err(|e| ApiError::internal(&e))?;

    // Resolve each name from catalog
    let mut resolved_servers = Vec::new();
    let mut not_found = Vec::new();
    for name in &req.servers {
        if let Some(entry) = catalog.iter().find(|c| &c.name == name) {
            resolved_servers.push(entry.clone());
        } else {
            not_found.push(name.clone());
        }
    }

    if !not_found.is_empty() {
        return Err(ApiError::bad_request(&format!(
            "MCP servers not found in catalog: {}", not_found.join(", ")
        )));
    }

    // Load existing per-agent config
    let mut existing = agent_config::load_agent_config(&data_dir, &agent_id)
        .unwrap_or(None)
        .unwrap_or_default();

    // Update MCP servers field
    existing.mcp_servers = if resolved_servers.is_empty() {
        Some(Vec::new())
    } else {
        Some(resolved_servers.clone())
    };

    // Save per-agent config
    agent_config::save_agent_config(&data_dir, &agent_id, &existing)
        .map_err(|e| ApiError::internal(&format!("Failed to save config: {}", e)))?;

    // Push RuntimeConfigUpdate to connected agent if available
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    mcp_servers: if resolved_servers.is_empty() {
                        Some(Vec::new())
                    } else {
                        Some(resolved_servers)
                    },
                    max_output_tokens: None,
                    max_iterations: None,
                    temperature: None,
                    system_prompt_override: None,
                    active_tools: None,
                    shell_approval_threshold: None,
                    model: None,
                    provider: None,
                })
                .await;
            if !push_result {
                tracing::warn!(
                    agent_id = %agent_id,
                    "Failed to push MCP config update to connected agent"
                );
            }
        }
    }

    Ok(Json(AgentMcpServersResponse {
        agent_id,
        active_servers: req.servers,
    }))
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
            ready: false,
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

    #[test]
    fn test_normalize_shell_tools_bash_to_shell() {
        let tools = vec!["bash".to_string(), "file_read".to_string()];
        let result = normalize_shell_tools(&tools);
        assert!(result.contains(&"shell".to_string()));
        assert!(!result.contains(&"bash".to_string()));
        assert!(result.contains(&"file_read".to_string()));
    }

    #[test]
    fn test_normalize_shell_tools_powershell_to_shell() {
        let tools = vec!["powershell".to_string(), "http_request".to_string()];
        let result = normalize_shell_tools(&tools);
        assert!(result.contains(&"shell".to_string()));
        assert!(!result.contains(&"powershell".to_string()));
    }

    #[test]
    fn test_normalize_shell_tools_deduplicate_both() {
        let tools = vec!["bash".to_string(), "powershell".to_string()];
        let result = normalize_shell_tools(&tools);
        // Should only have one "shell" entry
        assert_eq!(result.iter().filter(|t| *t == "shell").count(), 1);
    }

    #[test]
    fn test_normalize_shell_tools_no_shell_variant() {
        let tools = vec!["file_read".to_string(), "http_request".to_string()];
        let result = normalize_shell_tools(&tools);
        assert_eq!(result, tools);
    }
}
