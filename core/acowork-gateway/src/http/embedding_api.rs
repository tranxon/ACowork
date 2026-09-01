//! HTTP API for embedding model management.
//!
//! Endpoints:
//! - GET /api/embedding-models — list available models with status
//! - POST /api/embedding-models/{id}/download — trigger model download
//! - POST /api/embedding-models/{id}/select — switch active model
//! - GET /api/embedding-models/{id}/status — get model download/load status
//! - DELETE /api/embedding-models/{id} — delete downloaded model files

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use crate::http::routes::AppState;
use crate::lifecycle::embed;

// ── Response types ─────────────────────────────────────────────────────

/// Model entry with status for the listing endpoint.
#[derive(Debug, Serialize)]
pub struct EmbeddingModelWithStatus {
    /// Model ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Embedding vector dimension.
    pub dimension: usize,
    /// Maximum input tokens.
    pub max_tokens: usize,
    /// Download size in MB.
    pub size_mb: u64,
    /// Supported languages.
    pub languages: Vec<String>,
    /// Pooling strategy.
    pub pooling_strategy: String,
    /// Whether this model is recommended.
    pub recommended: bool,
    /// Whether this model is currently loaded.
    pub loaded: bool,
    /// Download status: "not_downloaded", "downloaded", "loaded".
    pub status: String,
    /// Available ONNX variants (e.g., {"fp32": "onnx/model.onnx", "fp16": "onnx/model_fp16.onnx"}).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onnx_variants: Option<std::collections::HashMap<String, String>>,
}

/// Response for GET /api/embedding-models.
#[derive(Debug, Serialize)]
pub struct EmbeddingModelsResponse {
    /// List of models with their status.
    pub models: Vec<EmbeddingModelWithStatus>,
    /// Currently active model ID.
    pub active_model_id: Option<String>,
    /// Whether the embedding service is running.
    pub service_running: bool,
}

/// Response for model download/select actions.
#[derive(Debug, Serialize)]
pub struct EmbeddingModelActionResponse {
    pub model_id: String,
    pub status: String,
    pub message: String,
}

/// Request for model download.
#[derive(Debug, Deserialize)]
pub struct DownloadModelRequest {
    /// ONNX variant to download (fp32, fp16, int8). Defaults to server config.
    pub variant: Option<String>,
}

/// Request for model selection.
#[derive(Debug, Deserialize)]
pub struct SelectModelRequest {
    /// Whether to force selection even when the new model has a different
    /// dimension than the current one (which would require embedding rebuild).
    /// If false and dimensions differ, the request is rejected with a
    /// dimension_mismatch status.
    #[serde(default)]
    pub force: bool,
}

/// Agent info returned when dimension change requires migration.
#[derive(Debug, Serialize)]
pub struct MigrationAgentEntry {
    /// Agent ID
    pub agent_id: String,
    /// Agent display name
    pub name: String,
    /// Whether this agent is currently running (must be running for migration)
    pub is_running: bool,
    /// Whether this agent has active LLM sessions (must stop before migration)
    pub has_active_sessions: bool,
    /// Current migration status (None = not started)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_status: Option<String>,
}

/// Response for select_model when dimension changes and migration is required.
#[derive(Debug, Serialize)]
pub struct SelectModelMigrationResponse {
    pub model_id: String,
    pub status: String,
    pub message: String,
    /// New embedding dimension
    pub new_dimension: usize,
    /// Old embedding dimension
    pub old_dimension: Option<usize>,
    /// Agents that will need migration
    pub agents: Vec<MigrationAgentEntry>,
}

/// Request for starting migration.
#[derive(Debug, Deserialize)]
pub struct StartMigrationRequest {
    /// Agent IDs to migrate (empty or absent = all running agents)
    #[serde(default)]
    pub agent_ids: Vec<String>,
}

// ── Route handlers ─────────────────────────────────────────────────────

/// GET /api/embedding-models — list available embedding models with status.
pub async fn list_embedding_models(State(state): State<AppState>) -> impl IntoResponse {
    // Clone all needed data from the read lock, then drop it before
    // making external HTTP requests (which cross await points).
    let (service_running, active_model_id, embed_port, model_entries) = {
        let gw = state.gateway_state.read().await;
        let (sr, ami, ep) = match &gw.embed_process {
            Some(eps) => (true, eps.active_model_id.clone(), Some(eps.port)),
            None => (false, None, None),
        };
        let entries: Vec<_> = gw
            .resource_cache
            .embedding_models
            .models.to_vec();
        (sr, ami, ep, entries)
    };

    // Query all model statuses **concurrently** to avoid serial round-trips.
    let status_futures: Vec<_> = model_entries
        .iter()
        .map(|entry| {
            let id = entry.id.clone();
            let loaded = active_model_id.as_deref() == Some(&id);
            async move {
                if loaded {
                    return "loaded".to_string();
                }
                if let Some(port) = embed_port {
                    match embed::get_embed_model_status(port, &id).await {
                        Ok(body) => body
                            .get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "not_downloaded".to_string()),
                        Err(_) => "not_downloaded".to_string(),
                    }
                } else {
                    "service_not_running".to_string()
                }
            }
        })
        .collect();

    let statuses = join_all(status_futures).await;

    let models: Vec<EmbeddingModelWithStatus> = model_entries
        .iter()
        .zip(statuses)
        .map(|(entry, status)| {
            let loaded = active_model_id.as_deref() == Some(&entry.id);
            EmbeddingModelWithStatus {
                id: entry.id.clone(),
                name: entry.name.clone(),
                description: entry.description.clone(),
                dimension: entry.dimension,
                max_tokens: entry.max_tokens,
                size_mb: entry.size_mb,
                languages: entry.languages.clone(),
                pooling_strategy: format!("{:?}", entry.pooling_strategy).to_lowercase(),
                recommended: entry.recommended,
                loaded,
                status,
                onnx_variants: entry.onnx_variants.clone(),
            }
        })
        .collect();

    Json(EmbeddingModelsResponse {
        models,
        active_model_id,
        service_running,
    })
    .into_response()
}

/// POST /api/embedding-models/{id}/download — trigger model download.
pub async fn download_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(req): Json<DownloadModelRequest>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    // Check if embed service is running
    let port = match &gw.embed_process {
        Some(eps) => eps.port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(EmbeddingModelActionResponse {
                    model_id,
                    status: "error".to_string(),
                    message: "Embedding service is not running".to_string(),
                }),
            )
                .into_response();
        }
    };

    // Check model exists in registry
    if !gw
        .resource_cache
        .embedding_models
        .models
        .iter()
        .any(|m| m.id == model_id)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(EmbeddingModelActionResponse {
                model_id: model_id.clone(),
                status: "error".to_string(),
                message: format!("Model '{}' not found in registry", model_id),
            }),
        )
            .into_response();
    }

    drop(gw);

    // Trigger download via embed service (fire-and-forget)
    match embed::download_embed_model(port, &model_id, req.variant.as_deref()).await {
        Ok(()) => Json(EmbeddingModelActionResponse {
            model_id,
            status: "downloading".to_string(),
            message: "Download started".to_string(),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: format!("Download failed: {}", e),
            }),
        )
            .into_response(),
    }
}

/// POST /api/embedding-models/{id}/select — switch active embedding model.
///
/// When the new model has a different dimension than the currently active model,
/// the request is rejected with `dimension_mismatch` status unless `force: true`
/// is set in the request body. The caller should then confirm with the user
/// that a full embedding rebuild is acceptable, and retry with `force: true`.
pub async fn select_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(req): Json<SelectModelRequest>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    // Check if embed service is running
    let port = match &gw.embed_process {
        Some(eps) => eps.port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(EmbeddingModelActionResponse {
                    model_id,
                    status: "error".to_string(),
                    message: "Embedding service is not running".to_string(),
                }),
            )
                .into_response();
        }
    };

    // Check model exists in registry
    let model_entry = gw
        .resource_cache
        .embedding_models
        .models
        .iter()
        .find(|m| m.id == model_id);
    let new_dim = match model_entry {
        Some(entry) => entry.dimension,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(EmbeddingModelActionResponse {
                    model_id: model_id.clone(),
                    status: "error".to_string(),
                    message: format!("Model '{}' not found in registry", model_id),
                }),
            )
                .into_response();
        }
    };

    // B6: Dimension change detection — warn if dimensions differ
    let current_dim = gw
        .embed_process
        .as_ref()
        .and_then(|eps| eps.active_dimension);
    let dimension_changed = current_dim.is_some_and(|cur| cur != new_dim);
    let current_model_id = gw
        .embed_process
        .as_ref()
        .and_then(|eps| eps.active_model_id.clone());

    drop(gw);

    if dimension_changed && !req.force {
        return (
            StatusCode::CONFLICT,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "dimension_mismatch".to_string(),
                message: format!(
                    "New model dimension ({}) differs from current ({}). \
                     Switching requires rebuilding all memory embeddings. \
                     Set force=true to confirm.",
                    new_dim,
                    current_dim.unwrap_or(0)
                ),
            }),
        )
            .into_response();
    }

    // Trigger model load via embed service
    match embed::select_embed_model(port, &model_id).await {
        Ok(()) => {
            // Update GatewayState with new active model info
            let mut gw = state.gateway_state.write().await;
            if let Some(eps) = &mut gw.embed_process {
                eps.active_model_id = Some(model_id.clone());
                eps.active_dimension = Some(new_dim);
                eps.ready = true;
            }
            if let Some(cfg) = gw.config.as_mut() {
                cfg.embedding_model = Some(model_id.clone());
                if let Err(e) = cfg.save() {
                    tracing::warn!(error = %e, "Failed to persist embedding model selection");
                }
            }

            // If dimension changed, return migration info instead of pushing config.
            // Frontend will show migration queue UI; user confirms → POST start-migration.
            if dimension_changed {
                let agents: Vec<MigrationAgentEntry> = gw
                    .running_agents
                    .values()
                    .map(|info| MigrationAgentEntry {
                        agent_id: info.agent_id.clone(),
                        name: info.agent_id.clone(), // Name resolved later by frontend
                        is_running: true,
                        has_active_sessions: false, // Unknown until agent is queried
                        migration_status: info.migration.as_ref().map(|m| {
                            if m.done { "completed".to_string() }
                            else if m.error.is_some() { "failed".to_string() }
                            else { "pending".to_string() }
                        }),
                    })
                    .collect();

                // Also include installed but not running agents (for frontend info)
                let running_ids: std::collections::HashSet<&str> = gw
                    .running_agents
                    .keys()
                    .map(|s| s.as_str())
                    .collect();
                let mut all_agents = agents;
                for (aid, info) in &gw.installed_agents {
                    if !running_ids.contains(aid.as_str()) {
                        all_agents.push(MigrationAgentEntry {
                            agent_id: aid.clone(),
                            name: info.name.clone(),
                            is_running: false,
                            has_active_sessions: false,
                            migration_status: None,
                        });
                    }
                }

                drop(gw);

                // ADR-033: Trigger MQTT global resource republish after resource change.
                if let Some(ref trigger) = state.mqtt_publisher_trigger {
                    trigger.trigger();
                }

                tracing::info!(
                    model_id = %model_id,
                    dimension = new_dim,
                    old_dimension = current_dim,
                    agent_count = all_agents.len(),
                    "Embedding model switched — migration required"
                );

                return (
                    StatusCode::OK,
                    Json(SelectModelMigrationResponse {
                        model_id,
                        status: "migration_required".to_string(),
                        message: format!(
                            "Model loaded. Migration required: dimension changed {} → {}.",
                            current_dim.unwrap_or(0),
                            new_dim
                        ),
                        new_dimension: new_dim,
                        old_dimension: current_dim,
                        agents: all_agents,
                    }),
                )
                    .into_response();
            }

            drop(gw);

            // Sidecar endpoint push removed — ResourcePusher was a no-op.
            // Sidecar endpoint push removed - ResourcePusher was a no-op.
            // The MQTT publisher trigger below already republishes
            // `acowork/global/embedding_models` (which includes the active
            // model ID and dimension), so running agents pick up the change
            // on the next publish cycle.

            tracing::info!(
                model_id = %model_id,
                dimension = new_dim,
                dimension_changed,
                previous_model = ?current_model_id,
                "Embedding model switched (same dimension)"
            );

            // ADR-033: Trigger MQTT global resource republish after resource change.
            if let Some(ref trigger) = state.mqtt_publisher_trigger {
                trigger.trigger();
            }

            Json(EmbeddingModelActionResponse {
                model_id,
                status: "loaded".to_string(),
                message: "Model loaded and activated".to_string(),
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: format!("Failed to load model: {}", e),
            }),
        )
            .into_response(),
    }
}

/// GET /api/embedding-models/{id}/status — get model status.
pub async fn get_model_status(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    // Check model exists in registry
    if !gw
        .resource_cache
        .embedding_models
        .models
        .iter()
        .any(|m| m.id == model_id)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "model_id": model_id,
                "error": "Model not found in registry"
            })),
        )
            .into_response();
    }

    let port = match &gw.embed_process {
        Some(eps) => eps.port,
        None => {
            return Json(serde_json::json!({
                "model_id": model_id,
                "status": "service_not_running",
            }))
            .into_response();
        }
    };

    drop(gw);

    match embed::get_embed_model_status(port, &model_id).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "model_id": model_id,
                "error": format!("Failed to get status: {}", e)
            })),
        )
            .into_response(),
    }
}

/// Response for embedding model test.
#[derive(Debug, Serialize)]
pub struct EmbeddingTestResponse {
    /// Whether the test passed.
    pub success: bool,
    /// Model ID tested.
    pub model_id: Option<String>,
    /// Embedding dimension returned.
    pub dimension: Option<usize>,
    /// Inference latency in milliseconds.
    pub latency_ms: Option<u64>,
    /// Error message if failed.
    pub error: Option<String>,
}

/// POST /api/embedding-models/test — test the currently loaded embedding model.
///
/// Sends a sample sentence to the embed service and verifies a valid
/// embedding vector is returned. Reports latency and dimension.
pub async fn test_embedding_model(State(state): State<AppState>) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    let port = match &gw.embed_process {
        Some(eps) if eps.ready => eps.port,
        Some(_) => {
            return Json(EmbeddingTestResponse {
                success: false,
                model_id: None,
                dimension: None,
                latency_ms: None,
                error: Some("Embedding service is starting up, not ready yet".to_string()),
            })
            .into_response();
        }
        None => {
            return Json(EmbeddingTestResponse {
                success: false,
                model_id: None,
                dimension: None,
                latency_ms: None,
                error: Some("Embedding service is not running".to_string()),
            })
            .into_response();
        }
    };

    drop(gw);

    match embed::test_embed_model(port).await {
        Ok(result) => Json(EmbeddingTestResponse {
            success: result.success,
            model_id: result.model_id,
            dimension: result.dimension,
            latency_ms: result.latency_ms,
            error: result.error,
        })
        .into_response(),
        Err(e) => Json(EmbeddingTestResponse {
            success: false,
            model_id: None,
            dimension: None,
            latency_ms: None,
            error: Some(format!("Test request failed: {}", e)),
        })
        .into_response(),
    }
}

/// DELETE /api/embedding-models/{id} — delete downloaded model files.
///
/// Forwards the delete request to the embed service which removes
/// model files from disk. Refuses if the model is currently loaded.
pub async fn delete_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    // Check if embed service is running
    let port = match &gw.embed_process {
        Some(eps) => eps.port,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(EmbeddingModelActionResponse {
                    model_id,
                    status: "error".to_string(),
                    message: "Embedding service is not running".to_string(),
                }),
            )
                .into_response();
        }
    };

    // Check model exists in registry
    if !gw
        .resource_cache
        .embedding_models
        .models
        .iter()
        .any(|m| m.id == model_id)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(EmbeddingModelActionResponse {
                model_id: model_id.clone(),
                status: "error".to_string(),
                message: format!("Model '{}' not found in registry", model_id),
            }),
        )
            .into_response();
    }

    // Check if this is the active model
    let is_active = gw
        .embed_process
        .as_ref()
        .and_then(|eps| eps.active_model_id.as_deref())
        == Some(&model_id);
    if is_active {
        return (
            StatusCode::CONFLICT,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: "Cannot delete the currently active model. Switch to another model first."
                    .to_string(),
            }),
        )
            .into_response();
    }

    drop(gw);

    match embed::delete_embed_model(port, &model_id).await {
        Ok(body) => {
            // Check if the embed service returned an error
            if let Some(err_msg) = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                let status_code = if err_msg.contains("currently loaded")
                    || err_msg.contains("being downloaded")
                {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                return (
                    status_code,
                    Json(EmbeddingModelActionResponse {
                        model_id,
                        status: "error".to_string(),
                        message: err_msg.to_string(),
                    }),
                )
                    .into_response();
            }

            // ADR-033: Trigger MQTT global resource republish after resource change.
            if let Some(ref trigger) = state.mqtt_publisher_trigger {
                trigger.trigger();
            }

            Json(EmbeddingModelActionResponse {
                model_id,
                status: "deleted".to_string(),
                message: "Model files deleted successfully".to_string(),
            })
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: format!("Failed to delete model: {}", e),
            }),
        )
            .into_response(),
    }
}

// ── Migration endpoints ──────────────────────────────────────────────────

/// GET /api/embedding-models/migration-progress — get migration progress for all agents.
pub async fn get_migration_progress(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    let agents: Vec<serde_json::Value> = gw
        .running_agents
        .values()
        .filter_map(|info| {
            info.migration.as_ref().map(|m| {
                serde_json::json!({
                    "agent_id": info.agent_id,
                    "request_id": m.request_id,
                    "target_model_id": m.target_model_id,
                    "target_dimension": m.target_dimension,
                    "progress": m.progress.as_ref().map(|(rebuilt, scanned, errors, phase, label)| {
                        serde_json::json!({
                            "rebuilt": rebuilt,
                            "total_scanned": scanned,
                            "errors": errors,
                            "phase": phase,
                            "label": label,
                        })
                    }),
                    "done": m.done,
                    "error": m.error,
                })
            })
        })
        .collect();

    drop(gw);

    Json(serde_json::json!({
        "agents": agents,
    }))
}

/// POST /api/embedding-models/{id}/start-migration - start embedding migration for agents.
///
/// Restores the dimension-migration feature that was lost in the
/// gRPC→MQTT refactor (Bug3): reads the active embed config from the
/// Gateway state, enumerates the target running agents, and forwards a
/// `POST /memory/rebuild-embeddings` request to each agent's Runtime
/// localhost HTTP server (via the ADR-055 D3 endpoint registry). Each
/// agent's progress is recorded in `RunningAgentInfo.migration` and
/// surfaced through `GET /api/embedding-models/migration-progress`.
pub async fn start_migration(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(req): Json<StartMigrationRequest>,
) -> impl IntoResponse {
    tracing::info!(
        target: "migration_diag",
        model_id = %model_id,
        requested_agent_ids = ?req.agent_ids,
        "start_migration: entry"
    );

    // 1. Read the active embed config from the Gateway state.
    let gw = state.gateway_state.read().await;
    let embed_process_present = gw.embed_process.is_some();
    let embed_process_ready = gw.embed_process.as_ref().map(|e| e.ready).unwrap_or(false);
    let (embed_endpoint, embed_model_id, embed_dimension) = match &gw.embed_process {
        Some(eps) if eps.ready => {
            let model = eps.active_model_id.clone().unwrap_or_default();
            let dim = eps.active_dimension.unwrap_or(0);
            let endpoint = format!("http://{}:{}/v1", gw.advertise_host, eps.port);
            (endpoint, model, dim)
        }
        _ => {
            tracing::warn!(
                target: "migration_diag",
                model_id = %model_id,
                embed_process_present,
                embed_process_ready,
                "start_migration: embed_process not ready, aborting"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(EmbeddingModelActionResponse {
                    model_id,
                    status: "error".to_string(),
                    message: "Embedding service is not running or not ready".to_string(),
                }),
            )
                .into_response();
        }
    };

    if embed_model_id.is_empty() || embed_dimension == 0 {
        tracing::warn!(
            target: "migration_diag",
            model_id = %model_id,
            embed_model_id = %embed_model_id,
            embed_dimension,
            "start_migration: embed config incomplete, aborting"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: "Embedding service has no active model loaded".to_string(),
            }),
        )
            .into_response();
    }

    tracing::info!(
        target: "migration_diag",
        model_id = %model_id,
        embed_endpoint = %embed_endpoint,
        embed_model_id = %embed_model_id,
        embed_dimension,
        "start_migration: embed config resolved"
    );

    // 2. Enumerate target agents (requested ids, or all running agents).
    let target_ids: Vec<String> = if req.agent_ids.is_empty() {
        gw.running_agents.keys().cloned().collect()
    } else {
        req.agent_ids.clone()
    };
    let running_agents_count = gw.running_agents.len();
    let running_agents_keys: Vec<String> = gw.running_agents.keys().cloned().collect();
    drop(gw);

    tracing::info!(
        target: "migration_diag",
        model_id = %model_id,
        target_ids_count = target_ids.len(),
        target_ids = ?target_ids,
        running_agents_count,
        running_agents_keys = ?running_agents_keys,
        "start_migration: enumerated targets"
    );

    if target_ids.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "model_id": model_id,
                "status": "ok",
                "message": "No running agents to migrate",
                "results": [],
            })),
        )
            .into_response();
    }

    // 3. Initialise per-agent migration state (`done=false`,
    //    `progress=(0,0,0,"reembed","starting")`) so the desktop's first
    //    `/api/embedding-models/migration-progress` poll sees an "in flight"
    //    snapshot, then spawn one background `tokio::task` per agent to
    //    proxy the rebuild request to the Runtime and write the terminal
    //    state when it completes. The handler returns immediately — slow
    //    Runtimes can no longer block the Gateway HTTP request, and the
    //    desktop polling loop can now observe the `progress` tuple while
    //    the work is actually running (Bug-UX-2: the previous synchronous
    //    implementation wrote `done=true` before the first 2s polling cycle
    //    could observe an intermediate `progress` snapshot, so the panel
    //    showed "重建中…" with no numbers and the banner just disappeared).
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    for agent_id in &target_ids {
        // 3a. Initial state — `done=false, progress=(0,0,0,"reembed","starting")`.
        let request_id = uuid::Uuid::new_v4().to_string();
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(info) = gw.running_agents.get_mut(agent_id) {
                info.migration = Some(crate::gateway::state::AgentMigrationState {
                    request_id: request_id.clone(),
                    target_model_id: embed_model_id.clone(),
                    target_dimension: embed_dimension,
                    progress: Some((
                        0,
                        0,
                        0,
                        "reembed".to_string(),
                        "starting".to_string(),
                    )),
                    done: false,
                    error: None,
                });
            }
        }

        // 3b. Build the body the Runtime's `rebuild_embeddings` handler
        //     expects. The endpoint / model_id / dimension are cloned here
        //     because they are still needed for the spawned task below.
        let body = serde_json::json!({
            "endpoint": embed_endpoint.clone(),
            "model_id": embed_model_id.clone(),
            "dimension": embed_dimension,
        });
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();

        tracing::info!(
            target: "migration_diag",
            model_id = %model_id,
            agent_id = %agent_id,
            request_id = %request_id,
            body = %body,
            "start_migration: spawning background task to forward rebuild request to runtime"
        );

        // 3c. Spawn — `AppState: Clone` makes this safe; the spawned task
        //     owns its own copy. Content-Type is set explicitly on the
        //     forwarded headers (Fix3): the Runtime's `Json<RebuildEmbeddingsBody>`
        //     extractor rejects requests without `Content-Type: application/json`
        //     with 415 Unsupported Media Type.
        let state_for_task = state.clone();
        let agent_id_owned: String = agent_id.clone();
        let headers_for_task = headers.clone();
        let request_id_owned = request_id.clone();
        tokio::spawn(async move {
            tracing::info!(
                target: "migration_diag",
                agent_id = %agent_id_owned,
                request_id = %request_id_owned,
                "start_migration_bg: invoking runtime rebuild endpoint"
            );
            let resp = crate::http::proxy::proxy_to_runtime_with_method(
                &state_for_task,
                &agent_id_owned,
                "/memory/rebuild-embeddings",
                "",
                reqwest::Method::POST,
                Some(body_bytes),
                &headers_for_task,
            )
            .await;

            let status_code = resp.status();
            let body_bytes = match to_bytes(resp.into_body(), usize::MAX).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        target: "migration_diag",
                        agent_id = %agent_id_owned,
                        request_id = %request_id_owned,
                        error = %e,
                        "start_migration_bg: failed to read runtime response body"
                    );
                    axum::body::Bytes::new()
                }
            };
            let text = String::from_utf8_lossy(&body_bytes).to_string();
            let success = status_code.is_success();
            let error_msg: Option<String> = if success {
                None
            } else {
                Some(if text.is_empty() {
                    format!("HTTP {} (empty body)", status_code.as_u16())
                } else {
                    text.clone()
                })
            };

            // 3d. Best-effort parse of the Runtime's `RebuildReport` (see
            //     `core/acowork-runtime/src/usecases/memory_query.rs`). If
            //     parsing fails we still write a terminal state, just
            //     without a `progress` tuple — the desktop will see
            //     `done=true` regardless.
            let final_progress: Option<(u64, u64, u64, String, String)> = if success {
                serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| {
                        let rebuilt = v.get("rebuilt")?.as_u64()?;
                        let total = v.get("total_scanned")?.as_u64()?;
                        let errors = v
                            .get("errors")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0);
                        Some((
                            rebuilt,
                            total,
                            errors,
                            "reembed".to_string(),
                            "completed".to_string(),
                        ))
                    })
            } else {
                None
            };

            tracing::info!(
                target: "migration_diag",
                agent_id = %agent_id_owned,
                request_id = %request_id_owned,
                http_status = %status_code,
                success,
                progress = ?final_progress,
                error = ?error_msg,
                "start_migration_bg: completed, writing terminal state"
            );

            // 3e. Write terminal state — either `done=true,
            //     progress=(rebuilt,total,...)` on success or
            //     `done=false, error=Some(msg)` on failure.
            let mut gw = state_for_task.gateway_state.write().await;
            if let Some(info) = gw.running_agents.get_mut(&agent_id_owned) {
                if let Some(m) = info.migration.as_mut() {
                    m.progress = final_progress;
                    m.done = success;
                    m.error = error_msg;
                } else {
                    tracing::warn!(
                        target: "migration_diag",
                        agent_id = %agent_id_owned,
                        request_id = %request_id_owned,
                        "start_migration_bg: migration state was cleared before background task completed"
                    );
                }
            }
        });

        results.push(serde_json::json!({
            "agent_id": agent_id,
            "status": "queued",
            "message": "Migration enqueued; poll /api/embedding-models/migration-progress for progress",
        }));
    }

    tracing::info!(
        target: "migration_diag",
        model_id = %model_id,
        queued_count = target_ids.len(),
        "start_migration: handler returning 200 OK (work continues in background)"
    );

    Json(serde_json::json!({
        "model_id": model_id,
        "status": "ok",
        "message": format!("Migration queued for {} agent(s)", target_ids.len()),
        "results": results,
    }))
    .into_response()
}

// ── Router ─────────────────────────────────────────────────────────────

/// Build the embedding models API router.
pub fn embedding_routes() -> Router<AppState> {
    Router::new()
        .route("/api/embedding-models", get(list_embedding_models))
        .route("/api/embedding-models/test", post(test_embedding_model))
        .route("/api/embedding-models/{id}/download", post(download_model))
        .route("/api/embedding-models/{id}/select", post(select_model))
        .route("/api/embedding-models/{id}/status", get(get_model_status))
        .route("/api/embedding-models/{id}", delete(delete_model))
        .route("/api/embedding-models/migration-progress", get(get_migration_progress))
        .route("/api/embedding-models/{id}/start-migration", post(start_migration))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use tower::util::ServiceExt;

    /// Bug3 + Fix3 regression: `POST /api/embedding-models/{id}/start-migration`
    /// must forward a rebuild request to the Runtime's
    /// `/memory/rebuild-embeddings` endpoint WITH `Content-Type: application/json`.
    ///
    /// Why this test exists (see session chat for full timeline):
    /// - Earlier buggy code appended `/memory/rebuild-embeddings` to a runtime
    ///   endpoint string that already contained `/agents/{id}`, producing a URL
    ///   the Runtime matched to a different handler and returned 403
    ///   `invalid node token` from. The fix is to use `proxy_to_runtime_with_method`
    ///   (same path every other `/api/agents/{id}/memory/*` endpoint takes).
    /// - The first version of that fix still failed: `proxy_to_runtime_with_method`
    ///   was called with an empty `HeaderMap`, so the Runtime's `Json<T>`
    ///   extractor returned 415 Unsupported Media Type. The fix is to set
    ///   `Content-Type: application/json` explicitly on the proxied request.
    ///
    /// This test pins BOTH contracts. Without it, regression is silent and
    /// only shows up in the Desktop memory panel as "Migration started for 1
    /// agent(s) — then nothing" (same UI symptom as the original bug).
    #[tokio::test]
    async fn test_start_migration_forwards_rebuild_request_to_runtime() {
        // ── 1. Mock Runtime: capture every received request as a single
        //    string for assertion. Single fallback route handles every path.
        type Captured = (String, String, String, String); // (method, uri, headers_csv, body)
        let received: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let received_for_server = received.clone();
        let mock_runtime = axum::Router::new().fallback(
            axum::routing::any(move |req: axum::extract::Request| async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let headers_csv = req
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}={}",
                            k.as_str(),
                            v.to_str().unwrap_or("<non-ascii>")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let body_bytes =
                    axum::body::to_bytes(req.into_body(), usize::MAX)
                        .await
                        .unwrap_or_default();
                received_for_server.lock().unwrap().push((
                    method,
                    uri,
                    headers_csv,
                    String::from_utf8_lossy(&body_bytes).to_string(),
                ));
                // Realistic RebuildReport payload so start_migration can
                // serialise the response and the body contains a useful
                // success signal.
                axum::Json(serde_json::json!({
                    "total_scanned": 107,
                    "rebuilt": 95,
                    "skipped_no_embedding": 12,
                    "errors": 0,
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_runtime).await.unwrap();
        });

        // ── 2. Build AppState with:
        //    - embed_process ready (port 18080, bge-small-zh-v1.5, dim 512)
        //    - running_agents[com.test.architect] present
        //    - runtime_http_registry pointing at the mock server, with the
        //      "/agents/{id}" suffix that a real Runtime publishes on its
        //      retained http_endpoint topic.
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-start-migration-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut gw_state =
            crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        gw_state.embed_process = Some(crate::lifecycle::embed::EmbedProcessState {
            pid: 0,
            port: 18080,
            active_model_id: Some("bge-small-zh-v1.5".to_string()),
            active_dimension: Some(512),
            ready: true,
        });
        gw_state.running_agents.insert(
            "com.test.architect".to_string(),
            crate::gateway::state::RunningAgentInfo {
                agent_id: "com.test.architect".to_string(),
                pid: 9999,
                started_at: chrono::Utc::now(),
                workspace: "/tmp/test".to_string(),
                node_id: "local".to_string(),
                connected: true,
                ready: true,
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: Some(384), // stale → triggers migration path
                migration: None,
            },
        );

        let mut state = crate::http::routes::AppState::new(
            Arc::new(tokio::sync::RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry.write().await.register(
            "com.test.architect",
            &format!(
                "http://127.0.0.1:{}/agents/com.test.architect",
                mock_port
            ),
        );
        state.runtime_http_registry = Some(registry);

        // ── 3. Mount embedding_routes and fire the Desktop's exact request.
        //    Clone state first so we can inspect the post-spawn state after
        //    the handler returns (Bug-UX-2 regression: the new async path
        //    spawns a `tokio::task` to proxy the rebuild request and writes
        //    the terminal state from inside that task — this assertion is
        //    what pins that contract).
        let state_for_assertion = state.clone();
        let app = super::embedding_routes().with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/embedding-models/bge-small-zh-v1.5/start-migration")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp_status = resp.status();
        let resp_body_bytes =
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
        let resp_body_str = String::from_utf8_lossy(&resp_body_bytes).to_string();

        // ── 3a. Bug-UX-2 contract: handler must return 200 OK with
        //     `status:"ok"` and a `queued` message — the work is now
        //     deferred to a spawned background task, not awaited.
        assert!(
            resp_status == axum::http::StatusCode::OK,
            "start_migration should return 200 OK on success; got {} body={}",
            resp_status,
            resp_body_str,
        );
        let resp_json: serde_json::Value = serde_json::from_slice(&resp_body_bytes)
            .expect("response body should be valid JSON");
        assert_eq!(
            resp_json["status"], "ok",
            "handler must return status=\"ok\" for queued migrations, got body={}",
            resp_body_str
        );
        assert!(
            resp_json["message"]
                .as_str()
                .map(|s| s.contains("queued"))
                .unwrap_or(false),
            "handler message must mention \"queued\" (work continues in background), got body={}",
            resp_body_str
        );
        let results = resp_json["results"]
            .as_array()
            .expect("response must include results array");
        assert_eq!(
            results.len(),
            1,
            "exactly one result expected for the one target agent"
        );
        assert_eq!(
            results[0]["status"], "queued",
            "per-agent status must be \"queued\" until the spawned task finishes"
        );

        // ── 3b. Yield long enough for the spawned background task to call
        //     the mock runtime and write the terminal state. 200ms is
        //     comfortably above the localhost HTTP roundtrip + lock
        //     acquisition cost without making the test noticeably slow.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // ── 4. Verify the spawned task actually forwarded the rebuild
        //    request to the Runtime with the right headers and body. This
        //    is the Bug3 + Fix3 regression core — without it, the handler
        //    could silently swallow the rebuild (returning "queued" but
        //    never actually calling the runtime) and the Desktop would
        //    never recover from the missing-embedding state.
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "expected exactly 1 forwarded rebuild request; got: {:?}",
            got
        );
        let (method, uri, headers_csv, body) = &got[0];

        assert_eq!(method, "POST", "must be POST, got {}", method);

        // Bug3 regression: the registry endpoint already includes
        // `/agents/com.test.architect`, and proxy appends
        // `/memory/rebuild-embeddings`. Earlier buggy code appended to a
        // path that was missing the `/agents/{id}` segment and produced
        // a URL the runtime's `agents/{id}/memory/*` handler matched
        // and rejected with 403. The fix uses `proxy_to_runtime_with_method`
        // — assert the URL is what the proxy actually builds.
        assert_eq!(
            uri, "/agents/com.test.architect/memory/rebuild-embeddings",
            "proxy must build /agents/{{id}}/memory/rebuild-embeddings, got {}",
            uri
        );

        // Fix3 regression: the runtime's `Json<RebuildEmbeddingsBody>`
        // extractor rejects requests without `Content-Type: application/json`
        // with 415. Without this header, the Desktop sees
        // "Migration started for 1 agent(s) — then nothing" and the
        // rebuild silently never runs.
        assert!(
            headers_csv
                .to_lowercase()
                .contains("content-type=application/json"),
            "proxy must forward Content-Type: application/json, got headers: {}",
            headers_csv
        );

        // Body must carry the new embed config so the Runtime can build a
        // fresh provider and re-embed. Field order is JSON-defined; assert
        // on substring presence rather than exact match.
        assert!(
            body.contains("\"endpoint\":\"http://127.0.0.1:18080/v1\""),
            "body must include endpoint, got: {}",
            body
        );
        assert!(
            body.contains("\"model_id\":\"bge-small-zh-v1.5\""),
            "body must include model_id, got: {}",
            body
        );
        assert!(
            body.contains("\"dimension\":512"),
            "body must include dimension, got: {}",
            body
        );

        // ── 5. Bug-UX-2 contract: the spawned background task must have
        //    written the terminal `AgentMigrationState` (done=true,
        //    progress=(rebuilt,total,errors,"reembed","completed"), error=None).
        //    This is what proves the deferred work actually completed and
        //    the Desktop polling loop will see real numbers — not just a
        //    "queued" promise that quietly drops on the floor.
        let gw = state_for_assertion.gateway_state.read().await;
        let info = gw
            .running_agents
            .get("com.test.architect")
            .expect("test setup must insert com.test.architect into running_agents");
        let m = info
            .migration
            .as_ref()
            .expect("spawned task must populate AgentMigrationState");
        assert!(
            m.done,
            "migration.done must be true after spawned task completes, got progress={:?} error={:?}",
            m.progress,
            m.error
        );
        assert_eq!(
            m.error, None,
            "successful migration must have error=None, got {:?}",
            m.error
        );
        assert_eq!(
            m.progress,
            Some((
                95,
                107,
                0,
                "reembed".to_string(),
                "completed".to_string(),
            )),
            "spawned task must parse RebuildReport and store (rebuilt,total,errors,phase,label); got {:?}",
            m.progress
        );
    }
}
