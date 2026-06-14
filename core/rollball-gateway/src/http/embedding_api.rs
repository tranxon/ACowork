//! HTTP API for embedding model management.
//!
//! Endpoints:
//! - GET /api/embedding-models — list available models with status
//! - POST /api/embedding-models/{id}/download — trigger model download
//! - POST /api/embedding-models/{id}/select — switch active model
//! - GET /api/embedding-models/{id}/status — get model download/load status
//! - DELETE /api/embedding-models/{id} — delete downloaded model files

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
    Router,
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

// ── Route handlers ─────────────────────────────────────────────────────

/// GET /api/embedding-models — list available embedding models with status.
pub async fn list_embedding_models(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Clone all needed data from the read lock, then drop it before
    // making external HTTP requests (which cross await points).
    let (service_running, active_model_id, embed_port, model_entries) = {
        let gw = state.gateway_state.read().await;
        let (sr, ami, ep) = match &gw.embed_process {
            Some(eps) => (true, eps.active_model_id.clone(), Some(eps.port)),
            None => (false, None, None),
        };
        let entries: Vec<_> = gw.resource_cache.embedding_models.models.iter().cloned().collect();
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
    if !gw.resource_cache.embedding_models.models.iter().any(|m| m.id == model_id) {
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
    let model_entry = gw.resource_cache.embedding_models.models.iter().find(|m| m.id == model_id);
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
    let current_dim = gw.embed_process.as_ref().and_then(|eps| eps.active_dimension);
    let dimension_changed = current_dim.map_or(false, |cur| cur != new_dim);
    let current_model_id = gw.embed_process.as_ref().and_then(|eps| eps.active_model_id.clone());

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

            drop(gw);

            // B7: Push EmbeddingConfigUpdate to all running Runtimes
            if let Some(ref pusher) = state.pusher {
                pusher.push_embedding_config().await;
            }

            tracing::info!(
                model_id = %model_id,
                dimension = new_dim,
                dimension_changed,
                previous_model = ?current_model_id,
                "Embedding model switched"
            );

            let message = if dimension_changed {
                format!(
                    "Model loaded. Dimension changed ({} → {}), \
                     Runtime will rebuild memory embeddings automatically.",
                    current_dim.unwrap_or(0),
                    new_dim
                )
            } else {
                "Model loaded and activated".to_string()
            };

            Json(EmbeddingModelActionResponse {
                model_id,
                status: "loaded".to_string(),
                message,
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
    if !gw.resource_cache.embedding_models.models.iter().any(|m| m.id == model_id) {
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
pub async fn test_embedding_model(
    State(state): State<AppState>,
) -> impl IntoResponse {
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
    if !gw.resource_cache.embedding_models.models.iter().any(|m| m.id == model_id) {
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
    let is_active = gw.embed_process.as_ref().and_then(|eps| eps.active_model_id.as_deref()) == Some(&model_id);
    if is_active {
        return (
            StatusCode::CONFLICT,
            Json(EmbeddingModelActionResponse {
                model_id,
                status: "error".to_string(),
                message: "Cannot delete the currently active model. Switch to another model first.".to_string(),
            }),
        )
            .into_response();
    }

    drop(gw);

    match embed::delete_embed_model(port, &model_id).await {
        Ok(body) => {
            // Check if the embed service returned an error
            if let Some(err_msg) = body.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                let status_code = if err_msg.contains("currently loaded") || err_msg.contains("being downloaded") {
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
}