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
use crate::http::agent_config::{
    self, AvatarAssetEntry, AvatarAssetsResponse, AvatarConfigResponse, UpdateAvatarConfigRequest,
};
use crate::http::routes::{ApiError, AppState};
use crate::lifecycle::process::is_process_alive;
use crate::lifecycle::manager::SYSTEM_AGENT_ID;
use acowork_core::AgentManifest;

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
        // ADR-034: `PUT /api/agents/{id}/config` is a pure reverse-proxy to
        // Runtime's `PUT /agents/{id}/config`.  The route itself is
        // registered in `proxy::proxy_routes` so all Runtime endpoints
        // stay co-located; this comment is the trace for code review.
        // The Gateway used to re-parse the body, forward only
        // `builtin_tools`, and echo the rest of the fields back — that
        // left per-agent fields like `temperature` / `max_output_tokens`
        // invisible to the Runtime (user-visible as "改动不生效").  All
        // persistence + live-broadcast now lives in the Runtime, so the
        // Gateway just forwards the body unchanged.
        // (intentionally NOT calling `put(...)` here — see proxy.rs.)
        //
        // Win11-MCP-ToolsBugFix (2026-07): the same ADR-034 pattern now also
        // covers `GET/PUT /api/agents/{id}/mcp-servers` and
        // `GET/PUT /api/agents/{id}/search-config`. Previously these were
        // bespoke stubs in this module that returned 200 but never persisted
        // (`let _ = (..., resolved_servers)`), causing the user's Tools-panel
        // selection to silently disappear on the next tab remount. Routes
        // are now registered in `proxy::proxy_routes` — see comment there.
        .route(
            "/api/agents/{id}/search-providers",
            get(get_agent_search_providers),
        )
        // ADR-034: All Runtime endpoints live as pure reverse-proxy routes in
        // `proxy::proxy_routes`.  The routes that previously had bespoke
        // handlers here (`PUT /api/agents/{id}/config`,
        // `GET /api/agents/{id}/sessions/{session_id}/state`) used to
        // re-parse the body and re-emit a Gateway-side DTO — that was
        // the source of the "改动不生效" bug because the Gateway
        // dropped per-agent fields like `temperature` and
        // `max_output_tokens` instead of forwarding them to the Runtime.
        // ADR-017: Avatar runtime config endpoints (work when agent is stopped)
        .route(
            "/api/agents/{id}/avatar-config",
            get(get_avatar_config).put(update_avatar_config),
        )
        .route(
            "/api/agents/{id}/manifest/avatar-assets",
            get(list_avatar_assets),
        )
        .route(
            "/api/agents/{id}/avatar-file",
            get(get_avatar_file).delete(delete_avatar_file),
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
    /// Debug Protocol port hint (set when dev_mode is true and agent is running).
    ///
    /// ADR-048: no longer bound by Runtime as a WebSocket listener; kept
    /// for API stability and operator dashboards that surface this field.
    pub debug_port: Option<u16>,
    /// RFC3339 timestamp of the last user-driven interaction with this agent
    /// (chat_message / approval / question_answer / compact_context).
    /// `None` for agents the user has never interacted with. Drives the
    /// sidebar sort order: newest first within each running/stopped group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_interaction_at: Option<String>,
    /// ADR-033: Whether the agent is online per MQTT LWT (Last Will Testament).
    /// Derived from the AgentRegistry which tracks `acowork/agents/{id}/status`.
    /// `running` reflects the Gateway's process-level view (PID alive),
    /// while `mqtt_online` reflects the broker's protocol-level view (TCP connected).
    /// These can differ briefly during crash recovery (e.g. process alive but
    /// MQTT broker hasn't detected TCP drop yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt_online: Option<bool>,
    /// Wall-clock timestamp (RFC3339) the Runtime published the `sleeping`
    /// retained status — i.e. when the auto-sleep watcher exited the process.
    /// `None` for agents that are not currently sleeping. Lets the Desktop
    /// distinguish "auto-slept at HH:MM" from "manually stopped" /
    /// "crashed" — both of which would otherwise look identical (running=false,
    /// mqtt_online=false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleeping_at: Option<String>,
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

/// `GET /api/agents` — list all installed agents.
///
/// Sort order (sidebar contract):
/// 1. System agent (`com.acowork.system`) is always pinned to the top.
/// 2. Running agents come before stopped agents.
/// 3. Within each group, agents with `last_interaction_at` come first
///    sorted newest-first; agents that have never been interacted with
///    sink to the bottom of their group, ordered alphabetically by name.
pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentListResponse>> {
    let gw = state.gateway_state.read().await;

    // ADR-033: Read MQTT-based online status from AgentRegistry as a sub-status.
    // Must use .read().await — blocking_read() panics inside tokio runtime.
    let mqtt_online_set: std::collections::HashSet<String> = if let Some(ref reg) = state.agent_registry {
        let reg = reg.read().await;
        reg.online_agents().into_iter().collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut agents: Vec<AgentListResponse> = gw
        .installed_agents
        .values()
        .map(|info| {
            // Verify the process is actually alive (not just in running_agents)
            let running_info = gw.running_agents.get(&info.agent_id);
            let actually_running = running_info
                .map(|r| is_process_alive(r.pid))
                .unwrap_or(false);
            // `connected` is the broker-level "Runtime's MQTT client is
            // reachable" signal. Pull it from the AgentRegistry (which
            // observes `acowork/agents/{id}/status` retained messages)
            // rather than the per-PID `running_agents[id].connected` field
            // — the latter is leftover from the gRPC `handle_agent_hello`
            // path that ADR-040 removed, and is never updated. Fall back to
            // the legacy field when the registry is unavailable (tests).
            let connected = running_info.map(|r| r.connected).unwrap_or(false)
                || mqtt_online_set.contains(&info.agent_id);
            let ready = running_info.map(|r| r.ready).unwrap_or(false);
            let last_interaction_at = gw
                .get_interaction(&info.agent_id)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            // ADR-017: Use manifest avatar for list (gRPC query would be too slow).
            let (eff_avatar, eff_builtin, _) =
                resolve_avatar_from_manifest(&info.manifest);
            let mqtt_online = if state.agent_registry.is_some() {
                Some(mqtt_online_set.contains(&info.agent_id))
            } else {
                None
            };
            // Read `sleeping_at` from the registry so each agent gets its own
            // timestamp. Use `try_read()` to avoid stalling the request if
            // another task is holding the write lock; fall back to None on
            // contention — the Desktop just retries on the next poll.
            let sleeping_at = state
                .agent_registry
                .as_ref()
                .and_then(|reg| {
                    match reg.try_read() {
                        Ok(guard) => guard.sleeping_at(&info.agent_id).map(|t| {
                            t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                        }),
                        Err(_) => None,
                    }
                });
            AgentListResponse {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                display_name: info.manifest.display_name.clone(),
                role: info.manifest.role.clone(),
                avatar: eff_avatar,
                builtin_avatar: eff_builtin,
                version: info.version.clone(),
                running: actually_running,
                connected,
                ready,
                dev_mode: running_info.map(|r| r.dev_mode).unwrap_or(false),
                debug_port: running_info.and_then(|r| r.debug_port),
                last_interaction_at,
                mqtt_online,
                sleeping_at,
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
    drop(gw);
    sort_agent_list(&mut agents);
    Json(agents)
}

/// Stable sidebar sort. See [`list_agents`] docstring for ordering rules.
fn sort_agent_list(agents: &mut [AgentListResponse]) {
    agents.sort_by(|a, b| {
        // 1) System agent always first.
        let a_sys = a.agent_id == SYSTEM_AGENT_ID;
        let b_sys = b.agent_id == SYSTEM_AGENT_ID;
        if a_sys != b_sys {
            return if a_sys {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        // 2) Running group above stopped group.
        if a.running != b.running {
            return if a.running {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        // 3) Within a group: by last_interaction_at DESC; None last;
        //    fall back to name for stable, predictable ordering.
        match (&a.last_interaction_at, &b.last_interaction_at) {
            (Some(ta), Some(tb)) => tb.cmp(ta),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
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
    // ADR-017: Use manifest avatar for detail page.
    let (eff_avatar, eff_builtin, _) =
        resolve_avatar_from_manifest(&info.manifest);
    let resp = AgentDetailResponse {
        agent_id: info.agent_id.clone(),
        name: info.name.clone(),
        display_name: info.manifest.display_name.clone(),
        role: info.manifest.role.clone(),
        avatar: eff_avatar,
        builtin_avatar: eff_builtin,
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
    // Long-lived immutable cache: the avatar bytes for a given (agent_id,
    // manifest.avatar) tuple are stable until the package is re-installed.
    // The Desktop client appends `?v=<manifest.version>` to bust the cache
    // when the version changes, so a one-year `max-age` is safe and lets the
    // browser/WebView skip the conditional request entirely on repeat views.
    // `immutable` further tells caches the response body will never change
    // for the lifetime of the URL, so the user agent may skip revalidation
    // even when the user explicitly reloads the page.
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
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

// ── ADR-017: Avatar config helpers ─────────────────────────────────────

/// Resolve effective avatar from manifest only (no Runtime query).
///
/// Used by `list_agents` and `get_agent_detail` where querying each running
/// agent via gRPC would be too slow. The avatar-config endpoint does a
/// full gRPC roundtrip when the agent is running.
///
/// Returns `(avatar, builtin_avatar, source)`.
fn resolve_avatar_from_manifest(
    manifest: &AgentManifest,
) -> (Option<String>, Option<String>, &'static str) {
    if manifest.avatar.is_some() || manifest.builtin_avatar.is_some() {
        return (manifest.avatar.clone(), manifest.builtin_avatar.clone(), "manifest");
    }
    (None, None, "fallback")
}

/// Whitelisted image extensions for avatar files.
const AVATAR_FILE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];

/// Check if a relative path has an avatar-allowed extension.
fn has_avatar_extension(path: &str) -> bool {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext {
        Some(e) => AVATAR_FILE_EXTENSIONS.contains(&e.as_str()),
        None => false,
    }
}

/// Validate that a relative path stays within the install directory.
/// Returns the canonicalized absolute path or an error.
fn validate_path_within_install(
    install_path: &str,
    relative_path: &str,
) -> Result<std::path::PathBuf, (StatusCode, Json<ApiError>)> {
    let install_dir = std::path::Path::new(install_path);
    let canonical_install = std::fs::canonicalize(install_dir).map_err(|_| {
        ApiError::not_found("Install directory not found for agent")
    })?;
    let target = install_dir.join(relative_path);
    let canonical_target = std::fs::canonicalize(&target).map_err(|_| {
        ApiError::not_found(&format!(
            "File not found: {}",
            relative_path
        ))
    })?;
    if !canonical_target.starts_with(&canonical_install) {
        return Err(ApiError::bad_request(
            "Path traversal detected: path must stay within install directory",
        ));
    }
    Ok(canonical_target)
}

/// `GET /api/agents/{id}/avatar-config` — get effective avatar configuration.
///
/// When the agent is running, queries the Runtime via gRPC (QueryConfig →
/// ConfigSnapshot) for the current avatar config. When stopped, falls
/// back to manifest.toml defaults.
pub async fn get_avatar_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AvatarConfigResponse>, (StatusCode, Json<ApiError>)> {
    let (manifest, is_running) = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        let is_running = gw.running_agents.get(&agent_id).map(|r| r.ready).unwrap_or(false);
        (info.manifest.clone(), is_running)
    };

    // When running, query the Runtime for the current avatar config.
    // ADR-033: gRPC removed — avatar config is read from persisted cache file
    // on startup; live queries are not supported in MQTT mode.
    // Always fall back to manifest.
    let _ = is_running;

    // Stopped (or gRPC failed): fall back to manifest.
    let (avatar, builtin_avatar, source) = resolve_avatar_from_manifest(&manifest);
    Ok(Json(AvatarConfigResponse {
        agent_id,
        avatar,
        builtin_avatar,
        source: source.to_string(),
    }))
}

/// `PUT /api/agents/{id}/avatar-config` — update avatar configuration.
///
/// When the agent is running, pushes a `RuntimeConfigUpdate` via gRPC
/// so the Runtime persists the change to `agent_config.json`.
/// When stopped, updates `manifest.toml` directly.
pub async fn update_avatar_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAvatarConfigRequest>,
) -> Result<Json<AvatarConfigResponse>, (StatusCode, Json<ApiError>)> {
    let (manifest, is_running) = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        let is_running = gw.running_agents.get(&agent_id).map(|r| r.ready).unwrap_or(false);
        (info.manifest.clone(), is_running)
    };

    // Normalize: empty string = clear (None), non-empty = set, absent = don't change.
    // Setting avatar clears builtin_avatar and vice versa.
    let new_avatar = match &req.avatar {
        Some(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Some(None) // explicitly clear
            } else {
                Some(Some(trimmed.to_owned()))
            }
        }
        None => None, // field absent — don't change
    };
    let new_builtin = match &req.builtin_avatar {
        Some(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Some(None)
            } else {
                // Validate builtin_avatar format
                if !is_plausible_builtin_avatar_id(trimmed) {
                    return Err(ApiError::bad_request(&format!(
                        "Invalid builtin_avatar value '{}': expected 'icon-NN' or numeric 1-99",
                        trimmed
                    )));
                }
                Some(Some(trimmed.to_owned()))
            }
        }
        None => None,
    };

    // Apply mutual exclusivity: setting avatar clears builtin and vice versa.
    let (effective_avatar, effective_builtin) = {
        let mut av = new_avatar.clone();
        let mut ba = new_builtin.clone();
        if let Some(Some(_)) = &av {
            ba = Some(None);
        }
        if let Some(Some(_)) = &ba {
            av = Some(None);
        }
        (av, ba)
    };

    // Snapshot for return value (before consuming in push/persist below).
    let return_avatar = effective_avatar.clone();
    let return_builtin = effective_builtin.clone();
    let any_set = new_avatar.is_some() || new_builtin.is_some();

    if is_running {
        // ADR-033: gRPC removed — RuntimeConfigUpdate push is no longer
        // supported. Runtime reads config from agent_config.json on startup
        // and agent_config cache file for avatar. Persisting to the cache
        // file below is sufficient.
        if any_set {
            tracing::info!(
                agent_id = %agent_id,
                "Avatar config updated (persisted to cache, Runtime will pick up on restart)"
            );
        }
    }

    // ADR-017: Persist avatar to the Gateway's avatar cache file (not manifest.toml).
    // The cache file survives Gateway restarts and is the source of truth for
    // list_agents when the agent is stopped. Running agents also get a gRPC
    // push above; the Runtime persists to agent_config.json independently.
    if any_set {
        let data_dir = {
            let gw = state.gateway_state.read().await;
            gw.config
                .as_ref()
                .map(|c| std::path::PathBuf::from(&c.data_dir))
                .unwrap_or_else(|| std::path::PathBuf::from("./data"))
        };
        let cache_avatar = effective_avatar.flatten();
        let cache_builtin = effective_builtin.flatten();
        agent_config::update_avatar_in_cache(
            &data_dir,
            &agent_id,
            cache_avatar.clone(),
            cache_builtin.clone(),
        );

        // Update in-memory manifest so list_agents returns the new value.
        let mut gw = state.gateway_state.write().await;
        if let Some(info) = gw.installed_agents.get_mut(&agent_id) {
            info.manifest.avatar = cache_avatar;
            info.manifest.builtin_avatar = cache_builtin;
        }
    }

    // Return the effective avatar.
    let (avatar, builtin_avatar, source) = if is_running && any_set {
        // For running agents with changes, return the pushed values.
        let av = return_avatar.flatten();
        let ba = return_builtin.flatten();
        if av.is_none() && ba.is_none() {
            resolve_avatar_from_manifest(&manifest)
        } else {
            (av, ba, "runtime")
        }
    } else {
        resolve_avatar_from_manifest(&manifest)
    };

    Ok(Json(AvatarConfigResponse {
        agent_id,
        avatar,
        builtin_avatar,
        source: source.to_string(),
    }))
}

/// `GET /api/agents/{id}/manifest/avatar-assets` — list custom avatar files.
///
/// Scans `{install_path}/assets/` for files matching `avatar*.{ext}`.
/// Sort: `avatar.ext` first, then `avatar-XX.ext` numerically.
/// Does NOT require the agent to be running.
pub async fn list_avatar_assets(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AvatarAssetsResponse>, (StatusCode, Json<ApiError>)> {
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    let assets_dir = std::path::Path::new(&install_path).join("assets");
    let mut entries: Vec<(String, Option<u32>)> = Vec::new();

    if let Ok(read_dir) = std::fs::read_dir(&assets_dir) {
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let name = match file_name.to_str() {
                Some(n) => n,
                None => continue,
            };
            // Match avatar*.{png,jpg,jpeg,gif,webp,svg}
            let lower = name.to_ascii_lowercase();
            if !lower.starts_with("avatar") {
                continue;
            }
            if !has_avatar_extension(&lower) {
                continue;
            }
            // Extract numeric suffix for sorting: "avatar.ext" → None (first),
            // "avatar-XX.ext" → Some(XX)
            let stem = std::path::Path::new(&lower)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let sort_key = if stem == "avatar" {
                None
            } else if let Some(suffix) = stem.strip_prefix("avatar-") {
                suffix.parse::<u32>().ok()
            } else {
                None
            };
            entries.push((format!("assets/{}", name), sort_key));
        }
    }

    // Sort: avatar.* first, then avatar-XX.* numerically
    entries.sort_by(|a, b| match (a.1, b.1) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a_n), Some(b_n)) => a_n.cmp(&b_n),
    });

    let assets = entries
        .into_iter()
        .map(|(path, _)| AvatarAssetEntry {
            relative_path: path,
        })
        .collect();

    Ok(Json(AvatarAssetsResponse {
        agent_id,
        assets,
    }))
}

/// Query params for avatar-file endpoint.
#[derive(Debug, Deserialize)]
pub struct AvatarFileQuery {
    pub path: String,
}

/// `GET /api/agents/{id}/avatar-file?path=<relative>` — serve a custom avatar file.
///
/// Path traversal guard + extension whitelist. Returns image bytes.
/// Does NOT require the agent to be running.
pub async fn get_avatar_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<AvatarFileQuery>,
) -> Result<Response<Body>, (StatusCode, Json<ApiError>)> {
    let install_path = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        info.install_path.clone()
    };

    // Extension whitelist check
    if !has_avatar_extension(&query.path) {
        return Err(ApiError::bad_request(
            "Invalid file extension: only png, jpg, jpeg, gif, webp, svg are allowed",
        ));
    }

    // Path traversal guard
    let canonical_path = validate_path_within_install(&install_path, &query.path)?;

    let bytes = std::fs::read(&canonical_path).map_err(|e| {
        ApiError::not_found(&format!(
            "Failed to read avatar file: {}",
            e
        ))
    })?;

    let content_type = match std::path::Path::new(&query.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CACHE_CONTROL,
            "public, max-age=300",
        )
        .body(Body::from(bytes))
        .unwrap())
}

/// `DELETE /api/agents/{id}/avatar-file?path=<relative>` — delete a custom avatar file.
///
/// Path traversal guard + extension whitelist. If the deleted file was the
/// current avatar, clears that field too (via gRPC when running, manifest
/// when stopped).
pub async fn delete_avatar_file(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<AvatarFileQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let (install_path, manifest, _is_running) = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;
        let is_running = gw.running_agents.get(&agent_id).map(|r| r.ready).unwrap_or(false);
        (info.install_path.clone(), info.manifest.clone(), is_running)
    };

    // Extension whitelist check
    if !has_avatar_extension(&query.path) {
        return Err(ApiError::bad_request(
            "Invalid file extension: only png, jpg, jpeg, gif, webp, svg are allowed",
        ));
    }

    // Path traversal guard
    let canonical_path = validate_path_within_install(&install_path, &query.path)?;

    // Delete the file
    std::fs::remove_file(&canonical_path).map_err(|e| {
        ApiError::internal(&format!("Failed to delete avatar file: {}", e))
    })?;

    // If the deleted file was the current avatar, clear it.
    let needs_clear = manifest.avatar.as_deref() == Some(query.path.as_str());
    if needs_clear {
        // ADR-033: gRPC removed — RuntimeConfigUpdate push no longer supported.
        // Runtime reads avatar from agent_config.json on startup.
        tracing::info!(
            agent_id = %agent_id,
            "Avatar file deleted, clearing from cache (Runtime will pick up on restart)"
        );

        // ADR-017: Clear avatar in the Gateway's cache file for BOTH running
        // and stopped agents so the change survives a Gateway restart.
        let data_dir = {
            let gw = state.gateway_state.read().await;
            gw.config
                .as_ref()
                .map(|c| std::path::PathBuf::from(&c.data_dir))
                .unwrap_or_else(|| std::path::PathBuf::from("./data"))
        };
        agent_config::update_avatar_in_cache(&data_dir, &agent_id, None, None);

        // Update in-memory manifest.
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(info) = gw.installed_agents.get_mut(&agent_id) {
                info.manifest.avatar = None;
            }
        }
    }

    Ok(Json(serde_json::json!({
        "message": "Avatar file deleted",
        "path": query.path,
    })))
}
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
    /// Start in developer mode (enables Debug Protocol: HTTP RPC + MQTT events per ADR-048)
    pub dev_mode: bool,
}


/// `POST /api/agents/:id/start` — start an agent
pub async fn start_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<StartAgentRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    // Pre-flight checks — released before we touch the lifecycle so the
    // reaper task isn't starved while we read config.
    {
        let gw = state.gateway_state.read().await;
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
    }

    // Build lifecycle from current config.
    let (log_file_size_mb, log_file_count, mqtt_port) = {
        let gw = state.gateway_state.read().await;
        (
            gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10),
            gw.config.as_ref().map(|c| c.log_file_count).unwrap_or(20),
            gw.config.as_ref().and_then(|c| if c.mqtt.enabled { Some(c.mqtt.port) } else { None }),
        )
    };
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        log_file_size_mb,
        log_file_count,
        mqtt_port,
    );
    // `wire_reaper = true`: this is the long-lived daemon path — the
    // reaper must clean up `running_agents` if the Runtime exits
    // (auto-sleep, crash, manual stop).
    lifecycle
        .start_agent(&agent_id, &state.gateway_state, req.dev_mode, true)
        .await
        .map_err(|e| ApiError::internal(&format!("Start failed: {}", e)))?;

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
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        10,
        20,
        None,
    );
    lifecycle
        .stop_agent(&agent_id, &state.gateway_state)
        .await
        .map_err(|e| ApiError::internal(&format!("Stop failed: {}", e)))?;

    Ok(Json(MessageResponse {
        message: format!("Agent stopped: {}", agent_id),
    }))
}

/// `POST /api/agents/:id/restart-debug` — restart a running agent in debug mode
///
/// ADR-033: gRPC removed. Debug mode is now configured at agent start time
/// (via `POST /api/agents/{id}/start` with `dev_mode: true`). Restart-in-debug
/// requires a full process restart in MQTT mode.
pub async fn restart_agent_in_debug(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // Already in debug mode — no-op
    let already_in_debug = {
        let gw = state.gateway_state.read().await;
        gw.running_agents
            .get(&agent_id)
            .map(|info| info.dev_mode && info.debug_port.is_some())
            .unwrap_or(false)
    };
    if already_in_debug {
        let port = {
            let gw = state.gateway_state.read().await;
            gw.running_agents
                .get(&agent_id)
                .and_then(|info| info.debug_port)
                .unwrap_or(0)
        };
        return Ok(Json(MessageResponse {
            message: format!(
                "Agent {} is already in debug mode (port {})",
                agent_id, port
            ),
        }));
    }

    // ADR-033: gRPC removed. Restart-in-debug requires a full stop+start cycle.
    // Stop the agent first, then restart with dev_mode=true.
    let (log_file_size_mb, log_file_count, mqtt_port) = {
        let gw = state.gateway_state.read().await;
        (
            gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10),
            gw.config.as_ref().map(|c| c.log_file_count).unwrap_or(20),
            gw.config.as_ref().and_then(|c| if c.mqtt.enabled { Some(c.mqtt.port) } else { None }),
        )
    };
    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(
        log_file_size_mb,
        log_file_count,
        mqtt_port,
    );

    // Stop current process
    lifecycle
        .stop_agent(&agent_id, &state.gateway_state)
        .await
        .map_err(|e| ApiError::internal(&format!("Stop before debug restart failed: {}", e)))?;

    // Start with dev_mode=true (wire reaper: long-lived daemon path)
    lifecycle
        .start_agent(&agent_id, &state.gateway_state, true, true)
        .await
        .map_err(|e| ApiError::internal(&format!("Debug restart failed: {}", e)))?;

    // Bump Gateway's log level to DEBUG so the Settings UI reflects it.
    {
        let level = "debug";
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(config) = &mut gw.config {
                config.log_level = level.to_string();
            }
        }
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
/// Queries the Runtime for per-agent model/provider preferences (stored in
/// workspace/config/agent_model.json). Gateway does NOT decide defaults —
/// default model/provider selection is session-level logic owned by the Runtime.
/// If the Runtime has no preference configured, returns empty strings.
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

    // ADR-033: gRPC removed — per-agent model/provider preferences are
    // read from agent_config.json on startup. Live queries via gRPC
    // are no longer supported. Return empty — Runtime decides defaults.
    let _unused = &agent_id;
    let active_model: Option<String> = None;
    let active_provider: Option<String> = None;

    // If Runtime has no preference, return empty — let the Runtime/Session decide defaults.
    let provider = match active_provider {
        Some(ref ap) if !ap.is_empty() => ap.clone(),
        _ => {
            return Ok(Json(AgentModelResponse {
                provider: String::new(),
                model: String::new(),
                available_models: Vec::new(),
            }));
        }
    };

    // Look up provider config from resource_cache for available_models.
    let available_models: Vec<String> = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider)
        .map(|cfg| cfg.models.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default();

    let model = active_model
        .filter(|m| available_models.contains(m))
        .unwrap_or_default();

    Ok(Json(AgentModelResponse {
        provider,
        model,
        available_models,
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

// ── Search provider per-agent config ─────────────────────────────────

/// Response for per-agent search provider list
#[derive(Serialize)]
pub struct AgentSearchProvidersResponse {
    pub agent_id: String,
    /// All search providers with API keys configured (from Gateway resource cache)
    pub providers: Vec<acowork_core::protocol::SearchProviderListItem>,
}

/// `GET /api/agents/{id}/search-providers` — get search provider list for agent
///
/// Returns the search provider catalog from Gateway's resource cache.
/// This tells the frontend which providers have API keys configured.
///
/// Win11-MCP-ToolsBugFix: `GET/PUT /api/agents/{id}/search-config` (the user's
/// active-provider selection) USED TO live here as a stub that returned 200
/// but never persisted — selection silently reset on next Tools-tab remount.
/// Those two endpoints now reverse-proxy to the Runtime; see
/// `proxy::proxy_routes()` for the route registration.
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

// ── Tests ──────────────────────────────────────────────────────────────

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
            last_interaction_at: None,
            mqtt_online: None,
            sleeping_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("com.example.weather"));
        assert!(json.contains("Weather Agent"));
        assert!(json.contains("icon-05"));
        // last_interaction_at is None and skipped on serialization.
        assert!(!json.contains("last_interaction_at"));
        // sleeping_at is None and skipped on serialization.
        assert!(!json.contains("sleeping_at"));
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

    fn entry(id: &str, name: &str, running: bool, ts: Option<&str>) -> AgentListResponse {
        AgentListResponse {
            agent_id: id.to_string(),
            name: name.to_string(),
            display_name: None,
            role: None,
            avatar: None,
            builtin_avatar: None,
            version: "1.0.0".to_string(),
            running,
            connected: false,
            ready: false,
            dev_mode: false,
            debug_port: None,
            last_interaction_at: ts.map(|s| s.to_string()),
            mqtt_online: None,
            sleeping_at: None,
        }
    }

    #[test]
    fn sort_pins_system_agent_first() {
        let mut list = vec![
            entry("com.acowork.alice", "Alice", true, None),
            entry("com.acowork.system", "System", false, None),
            entry("com.acowork.bob", "Bob", true, Some("2026-06-18T00:00:00Z")),
        ];
        sort_agent_list(&mut list);
        assert_eq!(list[0].agent_id, "com.acowork.system");
    }

    #[test]
    fn sort_groups_running_before_stopped() {
        let mut list = vec![
            entry("com.acowork.stopped1", "Stopped 1", false, Some("2026-06-18T10:00:00Z")),
            entry("com.acowork.running1", "Running 1", true, None),
            entry("com.acowork.stopped2", "Stopped 2", false, None),
            entry("com.acowork.running2", "Running 2", true, Some("2026-06-18T09:00:00Z")),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        // Running group first, within group time-bearing agents come before None ones;
        // same rule for the stopped group.
        assert_eq!(
            order,
            vec![
                "com.acowork.running2",  // running, has time
                "com.acowork.running1",  // running, no time (last in running group)
                "com.acowork.stopped1",  // stopped, has time
                "com.acowork.stopped2",  // stopped, no time (last overall)
            ]
        );
    }

    #[test]
    fn sort_orders_within_group_by_recency_then_name() {
        let mut list = vec![
            entry("com.acowork.zzz", "Zzz", true, None),
            entry("com.acowork.aaa", "Aaa", true, None),
            entry("com.acowork.bbb", "Bbb", true, Some("2026-06-18T01:00:00Z")),
            entry("com.acowork.ccc", "Ccc", true, Some("2026-06-18T05:00:00Z")),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "com.acowork.ccc", // 05:00 (newest)
                "com.acowork.bbb", // 01:00
                "com.acowork.aaa", // None, name Aaa first
                "com.acowork.zzz", // None, name Zzz
            ]
        );
    }

    #[test]
    fn sort_falls_back_to_name_when_all_none() {
        let mut list = vec![
            entry("com.acowork.zzz", "Zzz", true, None),
            entry("com.acowork.aaa", "Aaa", true, None),
            entry("com.acowork.mmm", "Mmm", false, None),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        // running group first (alphabetical), then stopped group
        assert_eq!(
            order,
            vec![
                "com.acowork.aaa",
                "com.acowork.zzz",
                "com.acowork.mmm",
            ]
        );
    }
}
