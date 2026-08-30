//! Publish API HTTP handlers.
//!
//! ADR-055 Phase 3b: publish prepare/build are delegated to the node via
//! `NodePublishPrepare` / `NodePublishBuild` control commands. The node
//! reports the structured result in `NodeEvent.result_json` (JSON), which
//! the handlers re-parse into the HTTP response shapes. `install-locally`
//! delegates to the local node; `export` remains read-only (it only
//! locates an already-built package on the Gateway's machine — for remote
//! nodes file retrieval is a Phase 3 follow-up).

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};

use acowork_core::mqtt_proto::NodeEvent;

use crate::http::routes::{ApiError, AppState};

/// Build the publish API router
pub fn publish_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/publish/prepare", post(prepare_publish))
        .route("/api/agents/{id}/publish/build", post(build_publish))
        .route(
            "/api/agents/{id}/publish/install-locally",
            post(install_locally),
        )
        .route("/api/agents/{id}/publish/export", post(export_package))
}

// ── S4.2 Prepare ──────────────────────────────────────────────────────

/// Publish prepare request
#[derive(Debug, Deserialize)]
pub struct PrepareRequest {
    #[serde(default)]
    pub clean: bool,
}

/// Publish prepare response (mirrors the node's `PrepareResult` JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub checks: Vec<serde_json::Value>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub cleaned: bool,
}

/// `POST /api/agents/:id/publish/prepare` — delegated to the node.
pub async fn prepare_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<PrepareRequest>,
) -> Result<Json<PrepareResponse>, (StatusCode, Json<ApiError>)> {
    let node_id = node_id_of(&state, &agent_id).await?;
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    let event = node_control
        .publish_prepare(&node_id, &agent_id, req.clean)
        .await
        .map_err(|e| ApiError::internal(&format!("Publish prepare failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&agent_id, &event)
        .map_err(|e| ApiError::internal(&format!("Publish prepare failed: {}", e)))?;

    Ok(Json(parse_result::<PrepareResponse>(&event)?))
}

// ── S4.3 Build ────────────────────────────────────────────────────────

/// Build request
#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    #[serde(default)]
    pub sign: bool,
    #[serde(default)]
    pub key_dir: Option<String>,
}

/// Build response (mirrors the node's `BuildResult` JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct BuildResponse {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// `POST /api/agents/:id/publish/build` — delegated to the node.
pub async fn build_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<BuildRequest>,
) -> Result<Json<BuildResponse>, (StatusCode, Json<ApiError>)> {
    let node_id = node_id_of(&state, &agent_id).await?;
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    // Empty output_dir → the node builds into its own packages_dir.
    let event = node_control
        .publish_build(&node_id, &agent_id, "", req.sign, req.key_dir.as_deref().unwrap_or(""))
        .await
        .map_err(|e| ApiError::internal(&format!("Publish build failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&agent_id, &event)
        .map_err(|e| ApiError::internal(&format!("Publish build failed: {}", e)))?;

    Ok(Json(parse_result::<BuildResponse>(&event)?))
}

/// Resolve the node hosting an installed agent (publish operates on the
/// node that owns the package).
async fn node_id_of(
    state: &AppState,
    agent_id: &str,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    gw.installed_agents
        .get(agent_id)
        .map(|i| i.node_id.clone())
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))
}

/// Parse the structured result a node reported in `NodeEvent.result_json`.
fn parse_result<T: serde::de::DeserializeOwned>(
    event: &NodeEvent,
) -> Result<T, (StatusCode, Json<ApiError>)> {
    let json = event
        .result_json
        .as_deref()
        .ok_or_else(|| ApiError::internal("Node reply missing result_json"))?;
    serde_json::from_str::<T>(json)
        .map_err(|e| ApiError::internal(&format!("Failed to parse node result: {}", e)))
}

/// Install-locally request
#[derive(Debug, Deserialize)]
pub struct InstallLocallyRequest {
    /// Path to the built .agent package (from build_publish response)
    pub package_path: String,
}

/// `POST /api/agents/:id/publish/install-locally` — install built package
/// locally via the local node.
pub async fn install_locally(
    State(state): State<AppState>,
    Path(_agent_id): Path<String>,
    Json(req): Json<InstallLocallyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiError>)> {
    // Extract the manifest to route the install command and register cron.
    let manifest = crate::http::agents::extract_manifest_from_package(
        std::path::Path::new(&req.package_path),
    )
    .map_err(|e| ApiError::bad_request(&format!("{}", e)))?;
    let agent_id = manifest.agent_id.clone();

    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::internal("Node control plane unavailable (MQTT disabled)")
    })?;
    let event = node_control
        .install_agent(
            acowork_core::node::LOCAL_NODE_ID,
            &agent_id,
            &req.package_path,
            crate::http::agents::gateway_dev_mode(&state).await,
        )
        .await
        .map_err(|e| ApiError::internal(&format!("Install-locally failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&agent_id, &event)
        .map_err(|e| ApiError::internal(&format!("Install-locally failed: {}", e)))?;

    {
        let mut gw = state.gateway_state.write().await;
        crate::cron::register_agent_cron_triggers(&mut gw, &agent_id, &manifest);
    }

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "message": format!("Package installed locally: {}", agent_id),
            "agent_id": agent_id,
        })),
    ))
}

/// Export response
#[derive(Debug, Serialize)]
pub struct ExportInfo {
    pub status: String,
    pub output_path: String,
}

/// `POST /api/agents/:id/publish/export` — export built .agent file
pub async fn export_package(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<ExportInfo>, (StatusCode, Json<ApiError>)> {
    let (output_dir, version) = {
        let gw = state.gateway_state.read().await;
        let info = gw
            .installed_agents
            .get(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?;

        let output_dir = gw
            .config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./build"));

        (output_dir, info.version.clone())
    };

    let filename = format!("{}-{}.agent", agent_id, version);
    let output_path = output_dir.join(&filename);

    if !output_path.exists() {
        return Err(ApiError::not_found(&format!(
            "Built package not found at: {}. Run publish/build first.",
            output_path.display()
        )));
    }

    Ok(Json(ExportInfo {
        status: "ready".to_string(),
        output_path: output_path.to_string_lossy().to_string(),
    }))
}
