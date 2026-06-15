//! Publish API HTTP handlers
//!
//! S4.1: Clone — POST /api/agents/:id/clone
//! S4.2: Prepare — POST /api/agents/:id/publish/prepare
//! S4.3: Build — POST /api/agents/:id/publish/build
//! S4.3a: CLI Package command wired through Gateway

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};

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
    /// Whether to perform cleanup operations (remove dev, clear recordings, reset config)
    #[serde(default)]
    pub clean: bool,
}

/// Publish prepare response
#[derive(Debug, Serialize)]
pub struct PrepareResponse {
    pub checks: Vec<crate::package_manager::publish::CheckItem>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub cleaned: bool,
}

/// `POST /api/agents/:id/publish/prepare` — check and clean agent for publishing
pub async fn prepare_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<PrepareRequest>,
) -> Result<Json<PrepareResponse>, (StatusCode, Json<ApiError>)> {
    let result = tokio::task::spawn_blocking(move || {
        let mut gw = state.gateway_state.blocking_write();
        crate::package_manager::publish::prepare_publish(&agent_id, req.clean, &mut gw)
    })
    .await;

    match result {
        Ok(Ok(prep)) => Ok(Json(PrepareResponse {
            checks: prep.checks,
            warnings: prep.warnings,
            errors: prep.errors,
            cleaned: prep.cleaned,
        })),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Prepare failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Prepare task failed: {}", e))),
    }
}

// ── S4.3 Build ────────────────────────────────────────────────────────

/// Build request
#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    /// Whether to sign the package
    #[serde(default)]
    pub sign: bool,
    /// Path to signing keys directory (defaults to examples/.signing-keys)
    #[serde(default)]
    pub key_dir: Option<String>,
}

/// Build response
#[derive(Debug, Serialize)]
pub struct BuildResponse {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// `POST /api/agents/:id/publish/build` — build .agent package
pub async fn build_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<BuildRequest>,
) -> Result<Json<BuildResponse>, (StatusCode, Json<ApiError>)> {
    // Determine output directory from Gateway config
    let output_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./build"))
    };

    let key_dir = req.key_dir.map(std::path::PathBuf::from);
    let sign = req.sign;

    // Perform build in spawn_blocking (build_package does file I/O)
    let result = tokio::task::spawn_blocking(move || {
        let gw = state.gateway_state.blocking_read();
        crate::package_manager::publish::build_package(
            &agent_id,
            &output_dir,
            sign,
            key_dir.as_deref(),
            &gw,
        )
    })
    .await;

    match result {
        Ok(Ok(build)) => Ok(Json(BuildResponse {
            output_path: build.output_path,
            signed: build.signed,
            file_size: build.file_size,
        })),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!("Build failed: {}", e))),
        Err(e) => Err(ApiError::internal(&format!("Build task failed: {}", e))),
    }
}

/// Install-locally request
#[derive(Debug, Deserialize)]
pub struct InstallLocallyRequest {
    /// Path to the built .agent package (from build_publish response)
    pub package_path: String,
}

/// `POST /api/agents/:id/publish/install-locally` — install built package locally
pub async fn install_locally(
    State(state): State<AppState>,
    Path(_agent_id): Path<String>,
    Json(req): Json<InstallLocallyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ApiError>)> {
    let packages_dir = get_packages_dir(&state).await;
    let dev_mode = get_dev_mode(&state).await;

    let result = tokio::task::spawn_blocking(move || {
        let pkg_path = std::path::PathBuf::from(&req.package_path);
        let mut gw = state.gateway_state.blocking_write();
        crate::package_manager::install::install_package(
            &pkg_path,
            &packages_dir,
            &mut gw,
            dev_mode,
        )
    })
    .await;

    match result {
        Ok(Ok(info)) => Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({
                "message": format!("Package installed locally: {}", info.agent_id),
                "agent_id": info.agent_id,
            })),
        )),
        Ok(Err(e)) => Err(ApiError::bad_request(&format!(
            "Install-locally failed: {}",
            e
        ))),
        Err(e) => Err(ApiError::internal(&format!(
            "Install-locally task failed: {}",
            e
        ))),
    }
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

// ── Helpers ───────────────────────────────────────────────────────────

async fn get_packages_dir(state: &AppState) -> std::path::PathBuf {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.packages_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./packages"))
}

async fn get_dev_mode(state: &AppState) -> bool {
    let gw = state.gateway_state.read().await;
    gw.config.as_ref().map(|c| c.dev_mode).unwrap_or(false)
}
