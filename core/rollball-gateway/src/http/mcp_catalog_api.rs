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

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{delete, get},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::http::routes::{ApiError, AppState};
use crate::resource_cache;
use rollball_core::protocol::McpServerConfigDef;

/// Build the MCP catalog router
pub fn mcp_catalog_routes() -> Router<AppState> {
    Router::new()
        .route("/api/mcp-catalog", get(list_catalog).put(replace_catalog).post(add_catalog_entry))
        .route("/api/mcp-catalog/{name}", delete(remove_catalog_entry).put(update_catalog_entry))
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
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read MCP catalog: {}", e))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse MCP catalog: {}", e))
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
            (k.clone(), if is_sensitive { "••••".to_string() } else { v.clone() })
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

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/mcp-catalog` — list all MCP server definitions (env values masked)
pub async fn list_catalog(
    State(state): State<AppState>,
) -> Result<Json<McpCatalogResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir)
        .map_err(|e| ApiError::internal(&e))?;

    let sensitive_keywords = ["key", "token", "secret", "password"];
    let servers: Vec<McpCatalogEntryResponse> = catalog
        .iter()
        .map(|c| {
            let masked = mask_sensitive_env(c);
            let has_secrets = c.env.keys().any(|k| {
                let lower = k.to_lowercase();
                sensitive_keywords.iter().any(|kw| lower.contains(kw))
            });
            McpCatalogEntryResponse { config: masked, has_secrets }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `PUT /api/mcp-catalog` — replace the entire catalog
pub async fn replace_catalog(
    State(state): State<AppState>,
    Json(new_catalog): Json<Vec<McpServerConfigDef>>,
) -> Result<Json<McpCatalogResponse>, (StatusCode, Json<ApiError>)> {
    // Validate: no duplicate names
    let mut seen = std::collections::HashSet::new();
    for entry in &new_catalog {
        if !seen.insert(entry.name.clone()) {
            return Err(ApiError::bad_request(&format!(
                "Duplicate MCP server name: '{}'", entry.name
            )));
        }
        if entry.name.is_empty() {
            return Err(ApiError::bad_request("MCP server name must not be empty"));
        }
    }

    let data_dir = get_data_dir(&state).await?;
    save_mcp_catalog(&data_dir, &new_catalog)
        .map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &new_catalog);
    }

    // Hot-push MCP config to all running agents
    if let Some(ref pusher) = state.pusher { pusher.push_mcp_catalog().await; }

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
            McpCatalogEntryResponse { config: masked, has_secrets }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `POST /api/mcp-catalog` — add a single server entry
pub async fn add_catalog_entry(
    State(state): State<AppState>,
    Json(body): Json<AddCatalogEntryRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), (StatusCode, Json<ApiError>)> {
    if body.config.name.is_empty() {
        return Err(ApiError::bad_request("MCP server name must not be empty"));
    }

    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir)
        .map_err(|e| ApiError::internal(&e))?;

    // Check for duplicate name
    if catalog.iter().any(|c| c.name == body.config.name) {
        return Err(ApiError::bad_request(&format!(
            "MCP server '{}' already exists in catalog", body.config.name
        )));
    }

    let name = body.config.name.clone();
    catalog.push(body.config);
    save_mcp_catalog(&data_dir, &catalog)
        .map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config to all running agents
    if let Some(ref pusher) = state.pusher { pusher.push_mcp_catalog().await; }

    Ok((StatusCode::CREATED, Json(MessageResponse {
        message: format!("MCP server '{}' added to catalog", name),
    })))
}

/// `PUT /api/mcp-catalog/{name}` — update a single server entry
pub async fn update_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateCatalogEntryRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir)
        .map_err(|e| ApiError::internal(&e))?;

    // If the name is being changed, check for conflicts first
    if body.config.name != name {
        if catalog.iter().any(|c| c.name == body.config.name) {
            return Err(ApiError::bad_request(&format!(
                "MCP server '{}' already exists in catalog", body.config.name
            )));
        }
    }

    // Find the existing entry index
    let idx = catalog.iter().position(|c| c.name == name)
        .ok_or_else(|| ApiError::not_found(&format!(
            "MCP server '{}' not found in catalog", name
        )))?;

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
    };

    save_mcp_catalog(&data_dir, &catalog)
        .map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config to all running agents
    if let Some(ref pusher) = state.pusher { pusher.push_mcp_catalog().await; }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' updated in catalog", new_name),
    }))
}

/// `DELETE /api/mcp-catalog/{name}` — remove a server entry
pub async fn remove_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir)
        .map_err(|e| ApiError::internal(&e))?;

    let original_len = catalog.len();
    catalog.retain(|c| c.name != name);
    if catalog.len() == original_len {
        return Err(ApiError::not_found(&format!(
            "MCP server '{}' not found in catalog", name
        )));
    }

    save_mcp_catalog(&data_dir, &catalog)
        .map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config to all running agents
    if let Some(ref pusher) = state.pusher { pusher.push_mcp_catalog().await; }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' removed from catalog", name),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Get the data_dir from Gateway state
async fn get_data_dir(state: &AppState) -> Result<PathBuf, (StatusCode, Json<ApiError>)> {
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
            transport: rollball_core::protocol::McpTransportDef::Stdio,
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-github".to_string()],
            env: std::collections::HashMap::from([
                ("GITHUB_PERSONAL_ACCESS_TOKEN".to_string(), "ghp_abc123".to_string()),
                ("SOME_OTHER_VAR".to_string(), "visible_value".to_string()),
            ]),
            ..Default::default()
        };

        let masked = mask_sensitive_env(&config);
        assert_eq!(masked.env.get("GITHUB_PERSONAL_ACCESS_TOKEN"), Some(&"••••".to_string()));
        assert_eq!(masked.env.get("SOME_OTHER_VAR"), Some(&"visible_value".to_string()));
    }

    #[test]
    fn test_catalog_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rollball-test-mcp-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let catalog = vec![
            McpServerConfigDef {
                name: "filesystem".to_string(),
                transport: rollball_core::protocol::McpTransportDef::Stdio,
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
                ..Default::default()
            },
        ];

        save_mcp_catalog(&dir, &catalog).unwrap();
        let loaded = load_mcp_catalog(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "filesystem");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_catalog() {
        let dir = std::env::temp_dir().join(format!("rollball-test-mcp-catalog-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let loaded = load_mcp_catalog(&dir).unwrap();
        assert!(loaded.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
