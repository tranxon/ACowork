//! MCP Catalog HTTP API handlers
//!
//! Manages the global MCP server catalog — a shared registry of server
//! definitions (including credentials/API keys) that all agents can
//! selectively activate. Analogous to the Vault for LLM providers.
//!
//! - GET    /api/mcp-catalog         — list all catalog entries (env values masked)
//! - PUT    /api/mcp-catalog         — replace the entire catalog
//! - POST   /api/mcp-catalog         — add a single server entry
//! - DELETE /api/mcp-catalog/{name}   — remove a server entry
//! - POST   /api/mcp-catalog/probe   — probe a server config (health check)
//! - POST   /api/mcp-catalog/{name}/probe — probe an existing catalog entry

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::http::routes::{ApiError, AppState, OperationAck};
use crate::resource_cache;
use acowork_core::operation::{OperationRecord, OperationState};
use acowork_core::protocol::{
    InstallState, McpInstallSpec, McpServerConfigDef, McpTransportDef,
};

/// Build the MCP catalog router
pub fn mcp_catalog_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/mcp-catalog",
            get(list_catalog)
                .put(replace_catalog)
                .post(add_catalog_entry),
        )
        .route(
            "/api/mcp-catalog/probe",
            post(probe_server_config),
        )
        .route(
            "/api/mcp-catalog/install",
            post(install_server),
        )
        .route(
            "/api/mcp-catalog/install/{name}",
            get(install_check).post(install_catalog_entry),
        )
        .route(
            "/api/mcp-catalog/{name}",
            delete(remove_catalog_entry).put(update_catalog_entry),
        )
        .route(
            "/api/mcp-catalog/{name}/probe",
            post(probe_catalog_entry),
        )
}

// ── Persistence helpers ──────────────────────────────────────────────

/// Build the path to the MCP catalog file.
fn catalog_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("mcp_catalog.json")
}

/// Load the MCP catalog from disk.
/// Returns an empty Vec if the file does not exist.
pub fn load_mcp_catalog(data_dir: &std::path::Path) -> Result<Vec<McpServerConfigDef>, String> {
    let path = catalog_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read MCP catalog: {}", e))?;
    serde_json::from_str(&raw).map_err(|e| format!("Failed to parse MCP catalog: {}", e))
}

/// Save the MCP catalog to disk.
pub fn save_mcp_catalog(
    data_dir: &std::path::Path,
    catalog: &[McpServerConfigDef],
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(catalog)
        .map_err(|e| format!("Failed to serialize MCP catalog: {}", e))?;
    std::fs::write(catalog_path(data_dir), json)
        .map_err(|e| format!("Failed to write MCP catalog: {}", e))?;
    tracing::info!(count = catalog.len(), "MCP catalog saved");
    Ok(())
}

// ── Masking helper ───────────────────────────────────────────────────

/// Mask sensitive env values for API responses.
/// Returns a copy of the config with env values containing "key", "token",
/// "secret", or "password" in their key name replaced with "••••".
fn mask_sensitive_env(config: &McpServerConfigDef) -> McpServerConfigDef {
    let sensitive_keywords = ["key", "token", "secret", "password"];
    let masked_env: std::collections::HashMap<String, String> = config
        .env
        .iter()
        .map(|(k, v)| {
            let lower = k.to_lowercase();
            let is_sensitive = sensitive_keywords.iter().any(|kw| lower.contains(kw));
            (
                k.clone(),
                if is_sensitive {
                    "••••".to_string()
                } else {
                    v.clone()
                },
            )
        })
        .collect();

    McpServerConfigDef {
        name: config.name.clone(),
        transport: config.transport.clone(),
        url: config.url.clone(),
        command: config.command.clone(),
        args: config.args.clone(),
        env: masked_env,
        headers: config.headers.clone(),
        tool_timeout_secs: config.tool_timeout_secs,
        install: config.install.clone(),
    }
}

// ── Response types ───────────────────────────────────────────────────

/// Catalog entry response (env values with sensitive fields masked)
#[derive(Serialize)]
pub struct McpCatalogEntryResponse {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
    /// Whether this entry has sensitive env vars that are masked
    pub has_secrets: bool,
}

/// Full catalog response
#[derive(Serialize)]
pub struct McpCatalogResponse {
    pub servers: Vec<McpCatalogEntryResponse>,
}

/// Request to add a single MCP server entry
#[derive(Deserialize)]
pub struct AddCatalogEntryRequest {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
    /// ADR-059 §7.3: the BootstrapState `version` the client read
    /// before writing (optimistic concurrency — stale clients are
    /// rejected with `resource_version_conflict`). Absent → no
    /// precondition.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// Request to update a single MCP server entry
#[derive(Deserialize)]
pub struct UpdateCatalogEntryRequest {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// MCP probe response — result of a health check against an MCP server
#[derive(Serialize)]
pub struct McpProbeResponse {
    /// Whether the connection succeeded
    pub success: bool,
    /// Number of tools discovered
    pub tool_count: usize,
    /// Tool names discovered
    pub tools: Vec<String>,
    /// Error message if failed
    pub error: Option<String>,
    /// Probe duration in milliseconds
    pub duration_ms: u64,
}

// ── Install DTOs (ADR-072) ─────────────────────────────────────────────

/// Request to install a preset MCP server (ADR-072). Install runs, then on
/// success the derived spawn config is written to the catalog atomically
/// (install-then-add, so a failed install never leaves a broken entry).
#[derive(Deserialize)]
pub struct McpInstallRequest {
    /// MCP server name (catalog entry name, e.g. "docling").
    pub name: String,
    /// ADR-059 §7.3 optimistic-concurrency precondition (same as add).
    #[serde(default)]
    pub expected_version: Option<u64>,
    /// Declarative install spec (kind × spec × spawn shape).
    pub install: McpInstallSpec,
    /// Optional pre-derived spawn config (frontend may pass its own);
    /// otherwise Gateway derives it from `install.package`.
    #[serde(default)]
    pub spawn: Option<McpServerConfigDef>,
    /// Preset env (e.g. API keys). Merged into the derived spawn env so
    /// `$VAR` placeholders in spawn_args resolve during the health check.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// Pre-flight install check — what the installer *would* do and whether the
/// runtime is ready. Drives the Install button (enabled/blocked + guidance).
#[derive(Serialize)]
pub struct McpInstallCheckResponse {
    pub name: String,
    pub runtime_ready: bool,
    pub missing_runtime: Option<String>,
    pub install_hint: Option<String>,
    pub install_command: Option<Vec<String>>,
    pub spawn: McpServerConfigDef,
}

/// Install run response (ADR-072).
#[derive(Serialize)]
pub struct McpInstallRunResponse {
    pub name: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub install_duration_ms: u64,
    pub tool_count: Option<usize>,
    pub health_error: Option<String>,
    /// Derived spawn config — present once install & health check succeeded.
    pub spawn: Option<McpServerConfigDef>,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/mcp-catalog` — list all MCP server definitions (env values masked)
pub async fn list_catalog(
    State(state): State<AppState>,
) -> Result<Json<McpCatalogResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let sensitive_keywords = ["key", "token", "secret", "password"];
    let servers: Vec<McpCatalogEntryResponse> = catalog
        .iter()
        .map(|c| {
            let masked = mask_sensitive_env(c);
            let has_secrets = c.env.keys().any(|k| {
                let lower = k.to_lowercase();
                sensitive_keywords.iter().any(|kw| lower.contains(kw))
            });
            McpCatalogEntryResponse {
                config: masked,
                has_secrets,
            }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `PUT /api/mcp-catalog` — replace the entire catalog
pub async fn replace_catalog(
    State(state): State<AppState>,
    Json(new_catalog): Json<Vec<McpServerConfigDef>>,
) -> Result<Json<McpCatalogResponse>, ApiError> {
    // Validate: no duplicate names
    let mut seen = std::collections::HashSet::new();
    for entry in &new_catalog {
        if !seen.insert(entry.name.clone()) {
            return Err(ApiError::bad_request(&format!(
                "Duplicate MCP server name: '{}'",
                entry.name
            )));
        }
        if entry.name.is_empty() {
            return Err(ApiError::bad_request("MCP server name must not be empty"));
        }
    }

    let data_dir = get_data_dir(&state).await?;
    save_mcp_catalog(&data_dir, &new_catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &new_catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger.
    // (The old ResourcePusher stub has been removed.)

    // Return masked response
    let sensitive_keywords = ["key", "token", "secret", "password"];
    let servers: Vec<McpCatalogEntryResponse> = new_catalog
        .iter()
        .map(|c| {
            let masked = mask_sensitive_env(c);
            let has_secrets = c.env.keys().any(|k| {
                let lower = k.to_lowercase();
                sensitive_keywords.iter().any(|kw| lower.contains(kw))
            });
            McpCatalogEntryResponse {
                config: masked,
                has_secrets,
            }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `POST /api/mcp-catalog` — add a single server entry
pub async fn add_catalog_entry(
    State(state): State<AppState>,
    Json(body): Json<AddCatalogEntryRequest>,
) -> Result<(StatusCode, Json<OperationAck>), ApiError> {
    // ADR-059 §7.3: reject stale writers before touching the catalog.
    crate::http::routes::check_expected_version(&state, body.expected_version).await?;

    if body.config.name.is_empty() {
        return Err(ApiError::bad_request("MCP server name must not be empty"));
    }

    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    // Check for duplicate name
    if catalog.iter().any(|c| c.name == body.config.name) {
        return Err(ApiError::bad_request(&format!(
            "MCP server '{}' already exists in catalog",
            body.config.name
        )));
    }

    catalog.push(body.config);
    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config to all running agents — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    // ADR-059 §6: open a committed operation record so the client can
    // correlate this mutation by `operation_id` and observe the
    // resulting `resource_version`. The side effect (catalog + disk +
    // mcp_list cache) already completed synchronously.
    let resource_version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.mcp_list.version
    };
    let mut record = OperationRecord::new(body.expected_version.unwrap_or(0));
    record.state = OperationState::Committed;
    record.resource_version = Some(resource_version);
    let ack = OperationAck::from_record(&record);
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }

    Ok((StatusCode::CREATED, Json(ack)))
}

/// `PUT /api/mcp-catalog/{name}` — update a single server entry
pub async fn update_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateCatalogEntryRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    // If the name is being changed, check for conflicts first
    if body.config.name != name
        && catalog.iter().any(|c| c.name == body.config.name) {
            return Err(ApiError::bad_request(&format!(
                "MCP server '{}' already exists in catalog",
                body.config.name
            )));
        }

    // Find the existing entry index
    let idx = catalog.iter().position(|c| c.name == name).ok_or_else(|| {
        ApiError::not_found(&format!("MCP server '{}' not found in catalog", name))
    })?;

    // Preserve sensitive env values that were sent as "••••" (masked)
    // If the user didn't change a secret field, keep the old value
    let old_env = catalog[idx].env.clone();
    let merged_env: std::collections::HashMap<String, String> = body
        .config
        .env
        .into_iter()
        .map(|(k, v)| {
            if v == "••••" {
                // Keep the old value for this key
                let old_val = old_env.get(&k).cloned().unwrap_or_default();
                (k, old_val)
            } else {
                (k, v)
            }
        })
        .collect();

    let new_name = body.config.name.clone();
    catalog[idx] = McpServerConfigDef {
        name: body.config.name,
        transport: body.config.transport,
        url: body.config.url,
        command: body.config.command,
        args: body.config.args,
        env: merged_env,
        headers: body.config.headers,
        tool_timeout_secs: body.config.tool_timeout_secs,
        install: body.config.install,
    };

    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' updated in catalog", new_name),
    }))
}

/// `DELETE /api/mcp-catalog/{name}` — remove a server entry
pub async fn remove_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let original_len = catalog.len();
    catalog.retain(|c| c.name != name);
    if catalog.len() == original_len {
        return Err(ApiError::not_found(&format!(
            "MCP server '{}' not found in catalog",
            name
        )));
    }

    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' removed from catalog", name),
    }))
}

// ── Probe handlers ──────────────────────────────────────────────────

/// Substitute `$VAR_NAME` references in command args with values from the env map.
/// Only substitutes references that have a matching key in the env map.
fn substitute_env_vars_in_args(
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut result = arg.clone();
            for (key, value) in env {
                let pattern = format!("${}", key);
                result = result.replace(&pattern, value);
            }
            result
        })
        .collect()
}

/// Core probe logic: attempt to connect to an MCP server and return health info.
///
/// If the initial stdio connection fails with "invalid JSON-RPC response" (which
/// typically means the process started in HTTP mode instead of stdio), we
/// automatically retry in HTTP mode to give the user a clear diagnosis.
async fn do_probe(config: McpServerConfigDef) -> McpProbeResponse {
    let start = std::time::Instant::now();

    // Substitute $ENV_VAR references in args from env map
    let resolved_args = substitute_env_vars_in_args(&config.args, &config.env);
    let resolved_config = McpServerConfigDef {
        args: resolved_args,
        ..config.clone()
    };

    match acowork_mcp::McpClient::connect(resolved_config).await {
        Ok(client) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            let tools: Vec<String> = client.tools().into_iter().map(|t| t.name).collect();
            let tool_count = tools.len();
            // Disconnect cleanly
            client.disconnect().await;
            McpProbeResponse {
                success: true,
                tool_count,
                tools,
                error: None,
                duration_ms,
            }
        }
        Err(e) => {
            let err_msg = format!("{:#}", e);

            // Smart diagnosis: if stdio failed with "invalid JSON-RPC response",
            // the process likely started in HTTP mode. Try HTTP fallback to confirm.
            if config.transport == McpTransportDef::Stdio
                && err_msg.contains("invalid JSON-RPC response")
            {
                // Common HTTP MCP endpoints, extended with package-declared
                // ports (ADR-072 decision 5 — e.g. docling defaults to 8000).
                let mut http_ports = vec![3333u16, 3000, 8080];
                if let Some(install) = &config.install {
                    let pkg = &install.package.http_probe_ports;
                    if !pkg.is_empty() {
                        http_ports.extend(pkg.iter().copied());
                    }
                }
                let http_urls: Vec<String> = http_ports
                    .iter()
                    .map(|p| format!("http://127.0.0.1:{p}/mcp"))
                    .collect();
                for url in &http_urls {
                    let http_config = McpServerConfigDef {
                        transport: McpTransportDef::Http,
                        url: Some(url.clone()),
                        ..config.clone()
                    };
                    if let Ok(client) = acowork_mcp::McpClient::connect(http_config).await {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let tools: Vec<String> =
                            client.tools().into_iter().map(|t| t.name).collect();
                        let tool_count = tools.len();
                        client.disconnect().await;
                        return McpProbeResponse {
                            success: true,
                            tool_count,
                            tools,
                            error: None,
                            duration_ms,
                        };
                    }
                }

                // HTTP fallback also failed — give a clear diagnosis
                let duration_ms = start.elapsed().as_millis() as u64;
                return McpProbeResponse {
                    success: false,
                    tool_count: 0,
                    tools: Vec::new(),
                    error: Some(format!(
                        "{err_msg}\n\nThe server process started but did not respond on stdio. \
                         It may be running in HTTP mode instead. \
                         Try adding \"--stdio\" to the command arguments."
                    )),
                    duration_ms,
                };
            }

            let duration_ms = start.elapsed().as_millis() as u64;
            McpProbeResponse {
                success: false,
                tool_count: 0,
                tools: Vec::new(),
                error: Some(err_msg),
                duration_ms,
            }
        }
    }
}

/// `POST /api/mcp-catalog/probe` — probe an MCP server config (health check)
///
/// Accepts a full MCP server config, attempts to connect and perform the
/// MCP initialize handshake + tools/list. Returns success/failure with
/// tool names and duration. Used by the frontend to verify a server
/// config is valid before saving it to the catalog.
pub async fn probe_server_config(
    Json(config): Json<McpServerConfigDef>,
) -> Result<Json<McpProbeResponse>, ApiError> {
    tracing::info!(server = %config.name, transport = ?config.transport, "Probing MCP server config");
    Ok(Json(do_probe(config).await))
}

/// `POST /api/mcp-catalog/{name}/probe` — probe an existing catalog entry
///
/// Loads the real (unmasked) config from the catalog file and probes it.
/// Used to re-verify an already-saved server's health.
pub async fn probe_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<McpProbeResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let config = catalog
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            ApiError::not_found(&format!("MCP server '{}' not found in catalog", name))
        })?;

    tracing::info!(server = %name, "Probing existing catalog MCP server");
    Ok(Json(do_probe(config).await))
}

// ── Install handlers (ADR-072) ─────────────────────────────────────────

/// Core install pipeline shared by `install_server` and `install_catalog_entry`.
///
/// 1. Runtime dependency check (structured guidance — no auto-install).
/// 2. Run the derived install command (if any).
/// 3. Derive spawn config & health check (single handshake, one retry for
///    slow first-run downloads).
async fn run_install_pipeline(
    name: &str,
    install: &McpInstallSpec,
    spawn_override: Option<McpServerConfigDef>,
    env: &std::collections::HashMap<String, String>,
) -> Result<(McpInstallRunResponse, McpServerConfigDef), ApiError> {
    let pkg = install.package.clone();

    // 1. Runtime dependency check.
    match acowork_mcp::ensure_runtime(&pkg) {
        Ok(()) => {}
        Err(acowork_mcp::DependencyStatus::Missing { runtime, install_hint }) => {
            return Err(ApiError::conflict(&format!(
                "Missing runtime '{}'. {} — install it first, then retry.",
                runtime, install_hint
            )));
        }
        Err(_) => return Err(ApiError::internal("unexpected runtime probe result")),
    }

    // 2. Run the install command (if any).
    let output = acowork_mcp::run_install(&pkg)
        .await
        .map_err(|e| ApiError::internal(&e))?;

    if !output.success {
        return Ok((
            McpInstallRunResponse {
                name: name.to_string(),
                success: false,
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                install_duration_ms: output.duration_ms,
                tool_count: None,
                health_error: None,
                spawn: None,
            },
            McpServerConfigDef::default(),
        ));
    }

    // 3. Derive spawn & health check with one retry.
    let mut spawn = match spawn_override {
        Some(s) => s,
        None => acowork_mcp::derive_spawn_config(name, &pkg),
    };
    // Merge preset env (API keys) into the spawn env so `$VAR` placeholders
    // in spawn_args resolve during the health check.
    spawn.env = acowork_mcp::build_spawn_env(env);

    let mut health = acowork_mcp::health_check(&spawn).await;
    if health.is_err() {
        // First-run downloads (e.g. uvx pulling deps) can exceed MCP_RECV;
        // a cached retry usually starts in milliseconds.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        health = acowork_mcp::health_check(&spawn).await;
    }

    match health {
        Ok(tool_count) => Ok((
            McpInstallRunResponse {
                name: name.to_string(),
                success: true,
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                install_duration_ms: output.duration_ms,
                tool_count: Some(tool_count),
                health_error: None,
                spawn: Some(spawn.clone()),
            },
            spawn,
        )),
        Err(e) => Ok((
            McpInstallRunResponse {
                name: name.to_string(),
                success: false,
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                install_duration_ms: output.duration_ms,
                tool_count: None,
                health_error: Some(e),
                spawn: Some(spawn.clone()),
            },
            spawn,
        )),
    }
}

/// `POST /api/mcp-catalog/install` — install a preset MCP server and write
/// it to the catalog on success (install-then-add, ADR-072 decision 7).
///
/// Body: [`McpInstallRequest`] (`name` + declarative `install` spec).
/// On success returns the derived spawn config; the entry is persisted with
/// `install.state = Installed` so the frontend "Install" button hides.
pub async fn install_server(
    State(state): State<AppState>,
    Json(body): Json<McpInstallRequest>,
) -> Result<(StatusCode, Json<McpInstallRunResponse>), ApiError> {
    crate::http::routes::check_expected_version(&state, body.expected_version).await?;

    if body.name.is_empty() {
        return Err(ApiError::bad_request("MCP server name must not be empty"));
    }

    let data_dir = get_data_dir(&state).await?;
    let (resp, spawn) =
        run_install_pipeline(&body.name, &body.install, body.spawn, &body.env).await?;

    if !resp.success {
        return Ok((StatusCode::OK, Json(resp)));
    }

    // Write derived spawn + install state into the catalog (create or update).
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;
    let mut entry = spawn;
    entry.name = body.name.clone();
    entry.env = acowork_mcp::build_spawn_env(&body.env);
    entry.install = Some(McpInstallSpec {
        package: body.install.package.clone(),
        state: InstallState::Installed,
    });

    let created = if let Some(existing) = catalog.iter_mut().find(|c| c.name == body.name) {
        *existing = entry;
        false
    } else {
        catalog.push(entry);
        true
    };
    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(resp),
    ))
}

/// `GET /api/mcp-catalog/install/{name}` — pre-flight check for an existing
/// catalog entry (Repair path). Returns runtime readiness + derived spawn
/// + install command so the frontend can gate the Install button.
pub async fn install_check(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<McpInstallCheckResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let entry = catalog
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| ApiError::not_found(&format!("MCP server '{name}' not found in catalog")))?;

    let install = entry.install.clone().ok_or_else(|| {
        ApiError::bad_request(&format!("MCP server '{name}' has no install spec"))
    })?;
    let pkg = install.package;

    let spawn = acowork_mcp::derive_spawn_config(&name, &pkg);
    let install_command = acowork_mcp::derive_install_command(&pkg);

    match acowork_mcp::probe_runtime(&pkg) {
        acowork_mcp::DependencyStatus::Ready => Ok(Json(McpInstallCheckResponse {
            name,
            runtime_ready: true,
            missing_runtime: None,
            install_hint: None,
            install_command,
            spawn,
        })),
        acowork_mcp::DependencyStatus::Missing { runtime, install_hint } => {
            Ok(Json(McpInstallCheckResponse {
                name,
                runtime_ready: false,
                missing_runtime: Some(runtime),
                install_hint: Some(install_hint),
                install_command,
                spawn,
            }))
        }
    }
}

/// `POST /api/mcp-catalog/install/{name}` — repair/reinstall an existing
/// catalog entry. Uses the catalog's stored spawn config (may have been
/// user-edited) and refreshes `install.state` on success.
pub async fn install_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<McpInstallRunResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let entry = catalog
        .into_iter()
        .find(|c| c.name == name)
        .ok_or_else(|| ApiError::not_found(&format!("MCP server '{name}' not found in catalog")))?;
    let install = entry.install.clone().ok_or_else(|| {
        ApiError::bad_request(&format!("MCP server '{name}' has no install spec"))
    })?;

    let (resp, _spawn) = run_install_pipeline(
        &name,
        &install,
        Some(entry.clone()),
        &std::collections::HashMap::new(),
    )
    .await?;

    if !resp.success {
        return Ok(Json(resp));
    }

    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;
    if let Some(existing) = catalog.iter_mut().find(|c| c.name == name) {
        existing.install = Some(McpInstallSpec {
            package: install.package.clone(),
            state: InstallState::Installed,
        });
    }
    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(resp))
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Get the data_dir from Gateway state
async fn get_data_dir(state: &AppState) -> Result<PathBuf, ApiError> {
    let gw = state.gateway_state.read().await;
    Ok(gw
        .config
        .as_ref()
        .map(|c| PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| PathBuf::from("./data")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_sensitive_env() {
        let config = McpServerConfigDef {
            name: "github".to_string(),
            transport: acowork_core::protocol::McpTransportDef::Stdio,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            env: std::collections::HashMap::from([
                (
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    "ghp_abc123".to_string(),
                ),
                ("SOME_OTHER_VAR".to_string(), "visible_value".to_string()),
            ]),
            ..Default::default()
        };

        let masked = mask_sensitive_env(&config);
        assert_eq!(
            masked.env.get("GITHUB_PERSONAL_ACCESS_TOKEN"),
            Some(&"••••".to_string())
        );
        assert_eq!(
            masked.env.get("SOME_OTHER_VAR"),
            Some(&"visible_value".to_string())
        );
    }

    #[test]
    fn test_catalog_save_load_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("acowork-test-mcp-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let catalog = vec![McpServerConfigDef {
            name: "filesystem".to_string(),
            transport: acowork_core::protocol::McpTransportDef::Stdio,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
            ],
            ..Default::default()
        }];

        save_mcp_catalog(&dir, &catalog).unwrap();
        let loaded = load_mcp_catalog(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "filesystem");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_catalog() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-mcp-catalog-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let loaded = load_mcp_catalog(&dir).unwrap();
        assert!(loaded.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
