//! Agent management HTTP API handlers
//!
//! Implements the Agent CRUD and lifecycle endpoints:
//! - GET    /api/agents           — list all agents with status
//! - GET    /api/agents/:id       — get agent detail
//! - GET    /api/agents/:id/avatar — get agent's packaged avatar image
//! - POST   /api/agents/install  — install a .agent package
//! - POST   /api/agents/:id/clone — clone an agent (skeleton or full)
//! - DELETE /api/agents/:id       — uninstall an agent
//! - POST   /api/agents/:id/start — start an agent
//! - POST   /api/agents/:id/stop  — stop a running agent

use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{Response, StatusCode, header},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::http::agent_config::{self, AgentConfigResponse, UpdateAgentConfigRequest};
use crate::http::routes::{ApiError, AppState};
use crate::lifecycle::process::is_process_alive;
use acowork_core::AgentManifest;
use acowork_core::protocol::GatewayResponse;
use acowork_core::protocol::{AgentSearchConfig, McpServerConfigDef};

/// Build embed_config_json from GatewayState's embed_process info.
/// Returns None if the embedding service is not running or has no active model.
async fn build_embed_config_json(state: &AppState) -> Option<String> {
    let gw = state.gateway_state.read().await;
    match &gw.embed_process {
        Some(eps) if eps.active_model_id.is_some() => {
            let endpoint = format!("http://127.0.0.1:{}/v1", eps.port);
            Some(
                serde_json::json!({
                    "embed_endpoint": endpoint,
                    "embed_model_id": eps.active_model_id.clone().unwrap_or_default(),
                    "embed_dimension": eps.active_dimension.unwrap_or(0),
                })
                .to_string(),
            )
        }
        _ => None,
    }
}

/// Build the agent management router
pub fn agent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route(
            "/api/agents/{id}",
            get(get_agent_detail).delete(uninstall_agent),
        )
        .route("/api/agents/{id}/avatar", get(get_agent_avatar))
        .route(
            "/api/agents/{id}/manifest/avatar",
            post(update_agent_manifest_avatar),
        )
        .route(
            "/api/agents/{id}/manifest/file",
            post(upload_agent_file),
        )
        .route("/api/agents/install", post(install_agent))
        .route("/api/agents/{id}/clone", post(clone_agent))
        .route("/api/agents/{id}/start", post(start_agent))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route(
            "/api/agents/{id}/restart-debug",
            post(restart_agent_in_debug),
        )
        .route("/api/agents/{id}/model", get(get_agent_model))
        .route(
            "/api/agents/{id}/config",
            get(get_agent_config).put(update_agent_config),
        )
        .route(
            "/api/agents/{id}/mcp-servers",
            get(get_agent_mcp_servers).put(update_agent_mcp_servers),
        )
        .route(
            "/api/agents/{id}/search-providers",
            get(get_agent_search_providers),
        )
        .route(
            "/api/agents/{id}/search-config",
            get(get_agent_search_config).put(update_agent_search_config),
        )
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
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    /// Used as the default builtin avatar on first install when `avatar`
    /// (a packaged image path) is not set. The client normalises and
    /// validates this against its bundled icon set.
    pub builtin_avatar: Option<String>,
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
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
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
    /// Embedding service config (endpoint, active model id, dimension).
    /// None when the embed service is not running or no model is loaded.
    /// Updated reactively by the embed supervisor's SSE monitor.
    pub embed_config_json: Option<String>,
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

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents` — list all installed agents
pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentListResponse>> {
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
                builtin_avatar: info.manifest.builtin_avatar.clone(),
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
    if let Some(sr) = gw.running_agents.get("com.acowork.senior-engineer") {
        tracing::info!(
            "[DIAG] list_agents: senior-engineer running=true ready={} connected={}",
            sr.ready,
            sr.connected
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
    let info = gw
        .installed_agents
        .get(&agent_id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

    let running_info = gw.running_agents.get(&agent_id);
    // Verify the process is actually alive
    let actually_running = running_info
        .as_ref()
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
        builtin_avatar: info.manifest.builtin_avatar.clone(),
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
        embed_config_json: build_embed_config_json(&state).await,
    };
    Ok(Json(resp))
}

/// `GET /api/agents/:id/avatar` — serve the agent's packaged avatar image.
///
/// The avatar path in the manifest is a relative path inside the installed
/// package directory. We resolve it to `<install_path>/<avatar>` and stream
/// the file bytes with a content type derived from the extension.
///
/// Returns 404 if:
/// - the agent is not installed
/// - the manifest does not declare an `avatar` field
/// - the resolved file does not exist
/// - the resolved file escapes the install directory (path traversal guard)
pub async fn get_agent_avatar(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Response<Body>, (StatusCode, Json<ApiError>)> {
    let (install_path, avatar_rel) = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        let avatar = info.manifest.avatar.clone().ok_or_else(|| {
            ApiError::not_found(&format!("Agent '{}' has no packaged avatar", agent_id))
        })?;
        (info.install_path.clone(), avatar)
    };

    let install_dir = std::path::Path::new(&install_path);
    let avatar_path = install_dir.join(&avatar_rel);

    // Canonicalize both to detect path traversal (e.g. "../../etc/passwd").
    // If the install dir doesn't exist, fall through to 404.
    let canonical_install = match std::fs::canonicalize(install_dir) {
        Ok(p) => p,
        Err(_) => {
            return Err(ApiError::not_found(&format!(
                "Install directory not found for agent '{}'",
                agent_id
            )));
        }
    };
    let canonical_avatar = match std::fs::canonicalize(&avatar_path) {
        Ok(p) => p,
        Err(_) => {
            return Err(ApiError::not_found(&format!(
                "Avatar file not found for agent '{}': {}",
                agent_id, avatar_rel
            )));
        }
    };
    if !canonical_avatar.starts_with(&canonical_install) {
        tracing::warn!(
            "Avatar path traversal blocked: agent={} avatar={} resolved={}",
            agent_id,
            avatar_rel,
            canonical_avatar.display()
        );
        return Err(ApiError::not_found("Avatar path is outside the install directory"));
    }

    let bytes = std::fs::read(&canonical_avatar).map_err(|e| {
        tracing::warn!(
            "Failed to read avatar file '{}': {}",
            canonical_avatar.display(),
            e
        );
        ApiError::not_found(&format!("Failed to read avatar: {}", e))
    })?;

    let content_type = guess_avatar_content_type(&canonical_avatar);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(&format!("Failed to build avatar response: {}", e)))?;
    Ok(resp)
}

/// Best-effort MIME type detection for avatar files by extension.
/// Supports the formats documented in `docs/02-agent-package.md` (PNG, JPG).
fn guess_avatar_content_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Request body for `POST /api/agents/{id}/manifest/avatar`.
///
/// Either field is optional. Pass `null` (or an empty string) to remove a
/// previously set value. Omitting a field leaves it unchanged.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateAvatarRequest {
    /// Packaged image path (e.g. "assets/avatar.png"). Set to null/empty to remove.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Builtin avatar index (e.g. "icon-05"). Set to null/empty to remove.
    #[serde(default)]
    pub builtin_avatar: Option<String>,
}

/// `POST /api/agents/{id}/manifest/avatar` — update the avatar fields in the
/// agent's installed `manifest.toml`. Used by the Publish wizard to bake the
/// user's selection into the package before build.
///
/// Persists the in-memory `AgentInfo.manifest` AND writes the on-disk
/// `manifest.toml` so the next `build_publish` reads the updated value.
pub async fn update_agent_manifest_avatar(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAvatarRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Apply changes: empty string is treated the same as null (clear the field).
    let new_avatar = req
        .avatar
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let new_builtin_avatar = req
        .builtin_avatar
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Validate builtin_avatar: must match icon-NN or N. Backend is permissive
    // (the client is the source of truth for the icon set), but we reject
    // obviously malformed values so a typo doesn't silently leak into the
    // built package.
    if let Some(ref value) = new_builtin_avatar
        && !is_plausible_builtin_avatar_id(value) {
            return Err(ApiError::bad_request(&format!(
                "Invalid builtin_avatar value '{}': expected 'icon-NN' or numeric 1-99",
                value
            )));
        }

    let manifest_path = std::path::Path::new(&install_path).join("manifest.toml");

    // Read-modify-write the on-disk manifest. We do this synchronously because
    // publish flow is a single-user CLI operation.
    let manifest_toml = std::fs::read_to_string(&manifest_path).map_err(|e| {
        ApiError::not_found(&format!(
            "manifest.toml not found at {}: {}",
            manifest_path.display(),
            e
        ))
    })?;
    let mut manifest: AgentManifest = AgentManifest::from_toml(&manifest_toml).map_err(|e| {
        ApiError::internal(&format!("Failed to parse existing manifest.toml: {}", e))
    })?;
    if req.avatar.is_some() {
        manifest.avatar = new_avatar.clone();
    }
    if req.builtin_avatar.is_some() {
        manifest.builtin_avatar = new_builtin_avatar.clone();
    }
    let new_toml = manifest
        .to_toml()
        .map_err(|e| ApiError::internal(&format!("Failed to serialize manifest: {}", e)))?;
    std::fs::write(&manifest_path, new_toml).map_err(|e| {
        ApiError::internal(&format!(
            "Failed to write manifest.toml at {}: {}",
            manifest_path.display(),
            e
        ))
    })?;

    // Update the in-memory copy so the next list_agents/get_agent_detail
    // returns the new values without requiring a Gateway restart.
    {
        let mut gw = state.gateway_state.write().await;
        if let Some(info) = gw.installed_agents.get_mut(&agent_id) {
            if req.avatar.is_some() {
                info.manifest.avatar = new_avatar.clone();
            }
            if req.builtin_avatar.is_some() {
                info.manifest.builtin_avatar = new_builtin_avatar.clone();
            }
        }
    }

    Ok(Json(serde_json::json!({
        "message": "Manifest avatar fields updated",
        "agent_id": agent_id,
        "avatar": new_avatar,
        "builtin_avatar": new_builtin_avatar,
    })))
}

/// Loose syntactic check for builtin_avatar values. Accepts "icon-NN" with
/// 1-99, or bare numeric 1-99. The client is still the source of truth for
/// whether the ID corresponds to a bundled icon — this is just a guard
/// against obvious typos.
fn is_plausible_builtin_avatar_id(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if let Some(num) = lower.strip_prefix("icon-") {
        if let Ok(n) = num.parse::<u32>() {
            return (1..=99).contains(&n);
        }
        return false;
    }
    if let Ok(n) = lower.parse::<u32>() {
        return (1..=99).contains(&n);
    }
    false
}

/// `POST /api/agents/{id}/manifest/file?path=<relative>`
///
/// Write a single file into the agent's install directory at the given
/// relative path. Used by the Publish wizard to upload a custom avatar
/// image that the wizard then references from `manifest.toml`.
///
/// The relative path is restricted to plain image extensions
/// (png/jpg/jpeg/gif/webp/svg) and is canonicalised to prevent escape
/// from the install dir (path traversal guard).
pub async fn upload_agent_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(params): Query<UploadFileQuery>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    let relative = params.path.trim();
    if relative.is_empty() {
        return Err(ApiError::bad_request("Missing 'path' query parameter"));
    }

    // Whitelist image extensions — this endpoint is specifically for avatar
    // uploads, not arbitrary files. New use cases should add their own
    // endpoint with broader validation.
    let ext = std::path::Path::new(relative)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    let allowed = matches!(
        ext.as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("webp") | Some("svg")
    );
    if !allowed {
        return Err(ApiError::bad_request(&format!(
            "Unsupported file extension: {}. Allowed: png, jpg, jpeg, gif, webp, svg",
            ext.as_deref().unwrap_or("(none)")
        )));
    }

    let install_dir = std::path::Path::new(&install_path);
    let target_path = install_dir.join(relative);

    // Path traversal guard: canonicalise and ensure the target is inside
    // the install dir. If the install dir doesn't exist, fall through to 404.
    let canonical_install = std::fs::canonicalize(install_dir).map_err(|e| {
        ApiError::not_found(&format!(
            "Install directory not found for agent '{}': {}",
            agent_id, e
        ))
    })?;
    if let Some(parent) = target_path.parent() {
        // Best-effort: create parent directories if missing. This is needed
        // because the canonicalize check below requires the parent to exist.
        std::fs::create_dir_all(parent).ok();
    }
    let canonical_target = std::fs::canonicalize(target_path.parent().unwrap_or(install_dir))
        .map_err(|e| {
            ApiError::internal(&format!(
                "Failed to resolve target directory for avatar upload: {}",
                e
            ))
        })?;
    if !canonical_target.starts_with(&canonical_install) {
        tracing::warn!(
            "Agent file upload blocked: agent={} path={} resolved={}",
            agent_id,
            relative,
            canonical_target.display()
        );
        return Err(ApiError::bad_request("File path is outside the install directory"));
    }

    // Drain the multipart body. We only expect a single "file" field.
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let data = field.bytes().await.map_err(|e| {
                ApiError::bad_request(&format!("Failed to read file field: {}", e))
            })?;
            bytes = Some(data.to_vec());
            break;
        }
    }
    let bytes = bytes.ok_or_else(|| ApiError::bad_request("Missing required field: 'file'"))?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request("Uploaded file is empty"));
    }
    // 10 MB cap — avatars are small. Larger uploads likely indicate a misuse.
    if bytes.len() > 10 * 1024 * 1024 {
        return Err(ApiError::bad_request("Uploaded file exceeds 10 MB limit"));
    }

    std::fs::write(&target_path, &bytes).map_err(|e| {
        ApiError::internal(&format!(
            "Failed to write file '{}': {}",
            target_path.display(),
            e
        ))
    })?;

    Ok(Json(serde_json::json!({
        "message": "File uploaded",
        "agent_id": agent_id,
        "path": relative,
        "size": bytes.len(),
    })))
}

/// Query parameters for `upload_agent_file`.
#[derive(Debug, Deserialize)]
pub struct UploadFileQuery {
    /// Relative file path within the agent's install directory
    /// (e.g. "assets/avatar.png").
    pub path: String,
}

/// `POST /api/agents/install` — upload and install a .agent package.
pub async fn install_agent(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    let mut package_bytes: Option<Vec<u8>> = None;
    let mut request_dev_mode: Option<bool> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await.map_err(|e| {
                    ApiError::bad_request(&format!("Failed to read package field: {}", e))
                })?;
                package_bytes = Some(bytes.to_vec());
            }
            "dev_mode" => {
                let text = field.text().await.unwrap_or_default();
                request_dev_mode = Some(text == "true" || text == "1");
            }
            _ => {}
        }
    }

    let package_bytes =
        package_bytes.ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;

    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    let packages_dir = packages_dir_from_state(&state).await;
    let dev_mode = match request_dev_mode {
        Some(v) => v,
        None => gateway_dev_mode(&state).await,
    };

    let install_result = tokio::task::spawn_blocking(move || {
        let temp_file = std::env::temp_dir().join(format!(
            "acowork-install-{}-{}.agent",
            std::process::id(),
            timestamp_nanos(),
        ));

        if let Err(e) = std::fs::write(&temp_file, &package_bytes) {
            return Err(GatewayError::Package(format!(
                "Failed to write upload to temp file: {}",
                e
            )));
        }

        let result = crate::package_manager::install::install_package(
            &temp_file,
            &packages_dir,
            &mut state.gateway_state.blocking_write(),
            dev_mode,
        );

        let _ = std::fs::remove_file(&temp_file);

        result
    })
    .await;

    install_response(install_result)
}

async fn packages_dir_from_state(state: &AppState) -> std::path::PathBuf {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.packages_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
}

async fn gateway_dev_mode(state: &AppState) -> bool {
    let gw = state.gateway_state.read().await;
    gw.config.as_ref().map(|c| c.dev_mode).unwrap_or(false)
}

fn install_response(
    install_result: Result<
        Result<crate::gateway::state::AgentInfo, GatewayError>,
        tokio::task::JoinError,
    >,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    match install_result {
        Ok(Ok(info)) => Ok((
            StatusCode::CREATED,
            Json(MessageResponse {
                message: format!("Package installed: {}", info.agent_id),
            }),
        )),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Install failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Install task failed: {}", e))),
    }
}

fn timestamp_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
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
            CloneModeParam::Skeleton => crate::package_manager::clone::CloneMode::Skeleton,
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
                "Agent {} is running, stop it first",
                agent_id
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
        crate::package_manager::uninstall::uninstall_package(&agent_id, &packages_dir, &mut gw)
    })
    .await;

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
#[derive(Default)]
pub struct StartAgentRequest {
    /// Start in developer mode (enables Debug Protocol WebSocket)
    pub dev_mode: bool,
}


/// `POST /api/agents/:id/start` — start an agent
pub async fn start_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<StartAgentRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_installed(&agent_id) {
        return Err(ApiError::not_found(&format!(
            "Agent not found: {}",
            agent_id
        )));
    }
    if gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!(
            "Agent {} is already running",
            agent_id
        )));
    }

    // Use the lifecycle manager to start the agent
    let idle_timeout = 300; // Default idle timeout
    let grpc_addr = crate::grpc::server::default_grpc_addr();
    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
    let log_file_size_mb = gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10);
    let log_file_count = gw.config.as_ref().map(|c| c.log_file_count).unwrap_or(20);
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        idle_timeout,
        gateway_grpc_endpoint,
        log_file_size_mb,
        log_file_count,
    );
    lifecycle
        .start_agent(&agent_id, &mut gw, req.dev_mode)
        .await
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
                tracing::warn!(
                    "Failed to reload Gateway tracing filter for debug mode: {}",
                    e
                );
            } else {
                tracing::info!(
                    "Gateway log level set to {} (debug mode agent start)",
                    level
                );
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
        return Err(ApiError::bad_request(&format!(
            "Agent {} is not running",
            agent_id
        )));
    }

    let idle_timeout = 300;
    let grpc_addr = crate::grpc::server::default_grpc_addr();
    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        idle_timeout,
        gateway_grpc_endpoint,
        10,
        20,
    );
    lifecycle
        .stop_agent(&agent_id, &mut gw)
        .await
        .map_err(|e| ApiError::internal(&format!("Stop failed: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Agent stopped: {}", agent_id),
    }))
}

/// `POST /api/agents/:id/restart-debug` — restart a running agent in debug mode
///
/// Unlike stop→start (which kills and spawns a new process), this endpoint
/// pushes an `EnableDebugMode` message to the Runtime via gRPC. The Runtime
/// then atomically switches to debug mode without process restart, preserving
/// session state and avoiding frontend race conditions.
pub async fn restart_agent_in_debug(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let mut gw = state.gateway_state.write().await;

    if !gw.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!(
            "Agent {} is not running",
            agent_id
        )));
    }

    // Already in debug mode — no-op
    if let Some(info) = gw.running_agents.get(&agent_id)
        && info.dev_mode && info.debug_port.is_some() {
            return Ok(Json(MessageResponse {
                message: format!(
                    "Agent {} is already in debug mode (port {})",
                    agent_id,
                    info.debug_port.unwrap_or(0)
                ),
            }));
        }

    // Check gRPC session manager is available
    let grpc_mgr = state
        .grpc_session_mgr
        .as_ref()
        .cloned()
        .ok_or_else(|| ApiError::internal("gRPC session manager not available"))?;

    let idle_timeout = 300;
    let grpc_addr = crate::grpc::server::default_grpc_addr();
    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
    let log_file_size_mb = gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10);
    let log_file_count = gw.config.as_ref().map(|c| c.log_file_count).unwrap_or(20);
    let lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        idle_timeout,
        gateway_grpc_endpoint,
        log_file_size_mb,
        log_file_count,
    );

    lifecycle
        .restart_in_debug(&agent_id, &mut gw, &grpc_mgr)
        .await
        .map_err(|e| ApiError::internal(&format!("Restart in debug failed: {}", e)))?;

    // Bump Gateway's log level to DEBUG so the Settings UI reflects it.
    {
        let level = "debug";
        if let Some(config) = &mut gw.config {
            config.log_level = level.to_string();
        }
        drop(gw);
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = tracing_subscriber::EnvFilter::new(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!(
                    "Failed to reload Gateway tracing filter for debug mode: {}",
                    e
                );
            } else {
                tracing::info!("Gateway log level set to {} (restart-in-debug)", level);
            }
        }
    }

    Ok(Json(MessageResponse {
        message: format!("Agent restarted in debug mode: {}", agent_id),
    }))
}

/// `GET /api/agents/:id/model` — get the current active model for an agent
///
/// Reads the per-agent model preference from the workspace `agent_model.json` file.
/// If no per-agent preference exists, falls back to the Gateway config default_model,
/// then the Vault entry's default_model (models[0]).
pub async fn get_agent_model(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentModelResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    if !gw.installed_agents.contains_key(&agent_id) {
        return Err(ApiError::not_found(&format!(
            "Agent not found: {}",
            agent_id
        )));
    }

    // Resolve provider and models from Gateway config / Vault
    let default_provider = gw
        .config
        .as_ref()
        .and_then(|c| c.default_provider.as_deref())
        .map(|s| s.to_string())
        .or_else(|| gw.vault.list_providers().first().cloned());

    let provider_name = match default_provider {
        Some(name) => name,
        None => return Err(ApiError::not_found("No provider configured in Vault")),
    };

    let vault_entry = gw
        .vault
        .get_provider(&provider_name)
        .map_err(|e| ApiError::internal(&format!("Vault error: {}", e)))?;

    let config_default_model = gw.config.as_ref().and_then(|c| c.default_model.as_deref());

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
            let query = acowork_core::proto::server_message::Payload::QueryConfig(
                acowork_core::proto::QueryConfig {
                    request_id: uuid::Uuid::new_v4().to_string(),
                },
            );
            match crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await {
                Some(response) => {
                    if let Some(acowork_core::proto::client_message::Payload::ConfigSnapshot(
                        snap,
                    )) = response.payload
                    {
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
                        .is_some_and(|ext| ext == "md" || ext == "txt")
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
        Ok(toml_str) => match AgentManifest::from_toml(&toml_str) {
            Ok(manifest) => manifest.tools.iter().map(|t| t.name.clone()).collect(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// Write updated `[[tools]]` declarations back to manifest.toml.
///
/// **Deprecated (ADR-009)**: Gateway no longer writes to agent workspace files.
/// active_tools persistence is handled by Runtime ({work_dir}/config/agent_config.json).
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
        lines.push("[[tools]]".to_string());
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
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        // Guard: agent must be running and ready.
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(&format!(
                    "Agent '{}' is starting up, please wait",
                    agent_id
                )));
            }
        } else {
            return Err(ApiError::service_unavailable(&format!(
                "Agent '{}' is not started",
                agent_id
            )));
        }
        gw.config
            .as_ref()
            .map(|c| c.max_output_tokens_limit)
            .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS)
    };

    // Query Runtime workspace config via IPC (QueryConfig → ConfigSnapshot roundtrip).
    let (
        model,
        provider,
        max_output_tokens,
        max_iterations,
        temperature,
        system_prompt_override,
        shell_approval_threshold,
        mcp_servers,
        search_config_json,
    ) = if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let query = acowork_core::proto::server_message::Payload::QueryConfig(
            acowork_core::proto::QueryConfig {
                request_id: uuid::Uuid::new_v4().to_string(),
            },
        );
        match crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await {
            Some(response) => {
                if let Some(acowork_core::proto::client_message::Payload::ConfigSnapshot(snap)) =
                    response.payload
                {
                    (
                        snap.model,
                        snap.provider,
                        snap.max_output_tokens,
                        snap.max_iterations,
                        snap.temperature,
                        snap.system_prompt_override,
                        snap.shell_approval_threshold,
                        snap.mcp_servers_json,
                        snap.search_config_json,
                    )
                } else {
                    (None, None, None, None, None, None, None, vec![], None)
                }
            }
            None => (None, None, None, None, None, None, None, vec![], None),
        }
    } else {
        (None, None, None, None, None, None, None, vec![], None)
    };

    // Build the effective config from ConfigSnapshot data
    let active_mcp_servers: Vec<String> = mcp_servers
        .iter()
        .filter_map(|j| serde_json::from_str::<McpServerConfigDef>(j).ok())
        .map(|s| s.name)
        .collect();
    let search_config: Option<AgentSearchConfig> = search_config_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());

    let effective = AgentConfigResponse {
        agent_id,
        max_output_tokens,
        max_iterations,
        temperature,
        system_prompt: None,
        // Use model snap fields for active model/provider in response
        model,
        provider,
        system_prompt_override,
        shell_approval_threshold,
        active_mcp_servers,
        search_config,
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
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        // Guard: agent must be running and ready.
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(&format!(
                    "Agent '{}' is starting up, please wait",
                    agent_id
                )));
            }
        } else {
            return Err(ApiError::service_unavailable(&format!(
                "Agent '{}' is not started",
                agent_id
            )));
        }
        gw.config
            .as_ref()
            .map(|c| c.max_output_tokens_limit)
            .unwrap_or(agent_config::DEFAULT_MAX_OUTPUT_TOKENS)
    };

    let req_system_prompt_override = req.system_prompt_override.clone();
    let req_shell_approval_threshold = req.shell_approval_threshold;
    let req_mcp_servers = req.mcp_servers.clone();

    // Push RuntimeConfigUpdate to connected agent
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            tracing::info!(
                agent_id = %agent_id,
                conn_id = %conn_id,
                "Pushing RuntimeConfigUpdate (config) to agent"
            );
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    max_output_tokens: req.max_output_tokens,
                    max_iterations: req.max_iterations,
                    temperature: req.temperature,
                    system_prompt_override: req_system_prompt_override,
                    shell_approval_threshold: req_shell_approval_threshold
                        .map(|t| format!("{:?}", t).to_lowercase()),
                    mcp_servers: req_mcp_servers,
                    model: None,
                    provider: None,
                    search_config_json: None,
                    embed_config_json: build_embed_config_json(&state).await,
                })
                .await;
            if !push_result {
                tracing::warn!(
                    agent_id = %agent_id,
                    conn_id = %conn_id,
                    "Failed to push RuntimeConfigUpdate to connected agent (push_tx closed or missing)"
                );
            } else {
                tracing::info!(
                    agent_id = %agent_id,
                    "RuntimeConfigUpdate pushed successfully to agent"
                );
            }
        } else {
            tracing::warn!(
                agent_id = %agent_id,
                session_count = mgr.session_count(),
                authenticated_count = mgr.authenticated_count(),
                "Cannot push RuntimeConfigUpdate: agent not found in IPC session manager"
            );
        }
    } else {
        tracing::warn!(
            agent_id = %agent_id,
            "Cannot push RuntimeConfigUpdate: session_mgr is None (IPC session manager not initialized)"
        );
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
        shell_approval_threshold: req_shell_approval_threshold
            .map(|t| format!("{:?}", t).to_lowercase()),
        model: None,
        provider: None,
        active_mcp_servers: vec![],
        search_config: None,
        global_max_output_tokens,
    };

    Ok(Json(effective))
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
/// Returns the list of MCP server names that are currently active for this agent,
/// queried from Runtime via gRPC (QueryConfig → ConfigSnapshot).
pub async fn get_agent_mcp_servers(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentMcpServersResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(&format!(
                    "Agent '{}' is starting up, please wait",
                    agent_id
                )));
            }
        } else {
            return Err(ApiError::service_unavailable(&format!(
                "Agent '{}' is not started",
                agent_id
            )));
        }
    }

    // Query Runtime for MCP config via gRPC
    let active_servers: Vec<String> = if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let query = acowork_core::proto::server_message::Payload::QueryConfig(
            acowork_core::proto::QueryConfig {
                request_id: uuid::Uuid::new_v4().to_string(),
            },
        );
        match crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await {
            Some(response) => {
                if let Some(acowork_core::proto::client_message::Payload::ConfigSnapshot(snap)) =
                    response.payload
                {
                    // Parse JSON strings back to server names
                    snap.mcp_servers_json
                        .into_iter()
                        .filter_map(|s| serde_json::from_str::<McpServerConfigDef>(&s).ok())
                        .map(|s| s.name)
                        .collect()
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

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
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
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
            "MCP servers not found in catalog: {}",
            not_found.join(", ")
        )));
    }

    // Push RuntimeConfigUpdate to connected agent (Runtime persists per-agent config)
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            tracing::info!(
                agent_id = %agent_id,
                conn_id = %conn_id,
                mcp_server_count = resolved_servers.len(),
                "Pushing RuntimeConfigUpdate (MCP) to agent"
            );
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    mcp_servers: if resolved_servers.is_empty() {
                        Some(Vec::new())
                    } else {
                        Some(resolved_servers.clone())
                    },
                    max_output_tokens: None,
                    max_iterations: None,
                    temperature: None,
                    system_prompt_override: None,
                    shell_approval_threshold: None,
                    model: None,
                    provider: None,
                    search_config_json: None,
                    embed_config_json: build_embed_config_json(&state).await,
                })
                .await;
            if !push_result {
                tracing::warn!(
                    agent_id = %agent_id,
                    conn_id = %conn_id,
                    "Failed to push MCP config update to connected agent (push_tx closed or missing)"
                );
            } else {
                tracing::info!(
                    agent_id = %agent_id,
                    "MCP config update pushed successfully to agent"
                );
            }
        } else {
            tracing::warn!(
                agent_id = %agent_id,
                session_count = mgr.session_count(),
                authenticated_count = mgr.authenticated_count(),
                "Cannot push MCP config: agent not found in IPC session manager. "
            );
        }
    } else {
        tracing::warn!(
            agent_id = %agent_id,
            "Cannot push MCP config: session_mgr is None (IPC session manager not initialized)"
        );
    }

    Ok(Json(AgentMcpServersResponse {
        agent_id,
        active_servers: req.servers,
    }))
}

// ── Search provider per-agent config ─────────────────────────────────

/// Response for per-agent search provider list
#[derive(Serialize)]
pub struct AgentSearchProvidersResponse {
    pub agent_id: String,
    /// All search providers with API keys configured (from Gateway resource cache)
    pub providers: Vec<acowork_core::protocol::SearchProviderListItem>,
}

/// Response for per-agent search config
#[derive(Serialize, Deserialize)]
pub struct AgentSearchConfigResponse {
    #[serde(default)]
    pub agent_id: String,
    /// Active search providers with priority
    pub providers: Vec<AgentSearchProviderEntry>,
}

/// A single active search provider entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSearchProviderEntry {
    pub provider: String,
    pub priority: u32,
}

/// Request body for PUT /api/agents/{id}/search-config
#[derive(Deserialize)]
pub struct UpdateAgentSearchConfigRequest {
    pub providers: Vec<AgentSearchProviderEntry>,
}

/// `GET /api/agents/{id}/search-providers` — get search provider list for agent
///
/// Returns the search provider catalog from Gateway's resource cache.
/// This tells the frontend which providers have API keys configured.
pub async fn get_agent_search_providers(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentSearchProvidersResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    let gw = state.gateway_state.read().await;
    let providers = gw.resource_cache.search_list.providers.clone();

    Ok(Json(AgentSearchProvidersResponse {
        agent_id,
        providers,
    }))
}

/// `GET /api/agents/{id}/search-config` — get per-agent search provider config
///
/// Returns the agent's current agent_search.json (active providers + priorities).
pub async fn get_agent_search_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentSearchConfigResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists and is running
    {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
        if let Some(info) = gw.running_agents.get(&agent_id) {
            if !info.ready {
                return Err(ApiError::service_unavailable(&format!(
                    "Agent '{}' is starting up, please wait",
                    agent_id
                )));
            }
        } else {
            return Err(ApiError::service_unavailable(&format!(
                "Agent '{}' is not started",
                agent_id
            )));
        }
    }

    // Query Runtime for search config via gRPC ConfigSnapshot
    let mut providers = Vec::new();
    if let Some(ref grpc_mgr) = state.grpc_session_mgr {
        let query = acowork_core::proto::server_message::Payload::QueryConfig(
            acowork_core::proto::QueryConfig {
                request_id: uuid::Uuid::new_v4().to_string(),
            },
        );
        if let Some(response) =
            crate::http::memory_api::grpc_memory_roundtrip(grpc_mgr, &agent_id, query).await
            && let Some(acowork_core::proto::client_message::Payload::ConfigSnapshot(snap)) =
                response.payload
            {
                // Parse search_config_json if available
                if let Some(ref search_json) = snap.search_config_json
                    && let Ok(config) =
                        serde_json::from_str::<AgentSearchConfigResponse>(search_json)
                    {
                        providers = config.providers;
                    }
            }
    }

    Ok(Json(AgentSearchConfigResponse {
        agent_id,
        providers,
    }))
}

/// `PUT /api/agents/{id}/search-config` — update per-agent search provider config
///
/// Saves the agent's chosen search providers + priorities to agent_search.json
/// via RuntimeConfigUpdate push to the connected agent.
pub async fn update_agent_search_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAgentSearchConfigRequest>,
) -> Result<Json<AgentSearchConfigResponse>, (StatusCode, Json<ApiError>)> {
    // Verify agent exists
    {
        let gw = state.gateway_state.read().await;
        if !gw.installed_agents.contains_key(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    let providers_json = serde_json::to_string(&AgentSearchConfigResponse {
        agent_id: agent_id.clone(),
        providers: req.providers.clone(),
    })
    .map_err(|e| ApiError::internal(&format!("Failed to serialize search config: {}", e)))?;

    // Push RuntimeConfigUpdate to connected agent (Runtime persists agent_search.json)
    if let Some(ref session_mgr) = state.session_mgr {
        let mgr = session_mgr.lock().await;
        if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
            let push_result = session
                .push_message(GatewayResponse::RuntimeConfigUpdate {
                    mcp_servers: None,
                    max_output_tokens: None,
                    max_iterations: None,
                    temperature: None,
                    system_prompt_override: None,
                    shell_approval_threshold: None,
                    model: None,
                    provider: None,
                    search_config_json: Some(providers_json),
                    embed_config_json: build_embed_config_json(&state).await,
                })
                .await;
            if !push_result {
                tracing::warn!(
                    agent_id = %agent_id,
                    "Failed to push search config update to connected agent"
                );
            }
        }
    }

    Ok(Json(AgentSearchConfigResponse {
        agent_id,
        providers: req.providers,
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
            builtin_avatar: Some("icon-05".to_string()),
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
        assert!(json.contains("icon-05"));
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
    fn test_is_plausible_builtin_avatar_id() {
        // Accepted forms
        assert!(is_plausible_builtin_avatar_id("icon-05"));
        assert!(is_plausible_builtin_avatar_id("icon-1"));
        assert!(is_plausible_builtin_avatar_id("ICON-12"));
        assert!(is_plausible_builtin_avatar_id("5"));
        assert!(is_plausible_builtin_avatar_id("01"));
        assert!(is_plausible_builtin_avatar_id("99"));
        // Rejected forms
        assert!(!is_plausible_builtin_avatar_id("icon-100"));
        assert!(!is_plausible_builtin_avatar_id("icon-0"));
        assert!(!is_plausible_builtin_avatar_id("icon-foo"));
        assert!(!is_plausible_builtin_avatar_id("icon-"));
        assert!(!is_plausible_builtin_avatar_id("foo"));
        assert!(!is_plausible_builtin_avatar_id(""));
        assert!(!is_plausible_builtin_avatar_id("0"));
        assert!(!is_plausible_builtin_avatar_id("100"));
    }
}
