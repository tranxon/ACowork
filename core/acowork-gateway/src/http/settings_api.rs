//! Settings HTTP API — global runtime toggles that are not tied to a
//! specific provider, MCP server, or search engine.
//!
//! ADR-056: Hosts the `default_compact_model` endpoint. Lives in its own
//! module so future global settings (e.g. global embedding default,
//! auto-compaction thresholds) can be added alongside without polluting
//! `provider_api.rs`.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};

use acowork_core::protocol::CompactModelRef;

use crate::http::routes::{ApiError, AppState};
use crate::resource_cache;

/// Build the settings router. Mounted at `/api/settings`.
pub fn settings_routes() -> Router<AppState> {
    Router::new().route(
        "/api/settings/default-compact-model",
        get(get_default_compact_model).put(put_default_compact_model),
    )
}

// ── Response / request DTOs ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DefaultCompactModelResponse {
    /// Current global default, `None` when not configured.
    pub default_compact_model: Option<CompactModelRef>,
}

#[derive(Debug, Deserialize)]
pub struct PutDefaultCompactModelRequest {
    /// `null` clears the global default. Otherwise sets the (provider_id,
    /// model_id) pair.
    pub default_compact_model: Option<CompactModelRef>,
}

// ── Handlers ─────────────────────────────────────────────────────────

/// `GET /api/settings/default-compact-model` — read current value.
pub async fn get_default_compact_model(
    State(state): State<AppState>,
) -> Result<Json<DefaultCompactModelResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let current = gw
        .resource_cache
        .provider_list
        .default_compact_model
        .clone();
    Ok(Json(DefaultCompactModelResponse {
        default_compact_model: current,
    }))
}

/// `PUT /api/settings/default-compact-model` — set or clear.
///
/// Body: `{ "default_compact_model": { "provider_id": "...", "model_id": "..." } }`
/// or `{ "default_compact_model": null }` to clear.
///
/// 422 on invalid (provider_id unknown, model_id not in that provider).
pub async fn put_default_compact_model(
    State(state): State<AppState>,
    Json(body): Json<PutDefaultCompactModelRequest>,
) -> Result<Json<DefaultCompactModelResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.data_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    };

    let mut gw = state.gateway_state.write().await;

    // In-memory mutation with validation. `set_default_compact_model` bumps
    // `version` on success; we persist right after. Validation failure
    // (unknown provider_id / model_id not in that provider) → 422 per
    // ADR-056 §4.1.
    let prev = resource_cache::set_default_compact_model(
        &mut gw.resource_cache.provider_list,
        body.default_compact_model.clone(),
    )
    .map_err(|e| ApiError::unprocessable_entity(&e))?;

    // Persist to disk (the in-memory version bump is sufficient; we don't
    // call `persist_provider_cache` here because that would bump the version
    // a *second* time).
    if let Err(e) = resource_cache::save_provider_list(
        &data_dir,
        &gw.resource_cache.provider_list,
    ) {
        // Roll back in-memory mutation on disk failure so on-disk + memory
        // stay consistent. The setter already mutated `default_compact_model`
        // and bumped `version`; revert those.
        gw.resource_cache.provider_list.default_compact_model = prev.clone();
        // The version bump is monotonic and cannot be trivially reversed
        // without racing with concurrent updates, so we leave it. The next
        // legitimate save will replace it.
        return Err(ApiError::internal(&format!(
            "Failed to persist provider_list.json: {}",
            e
        )));
    }

    let current = gw
        .resource_cache
        .provider_list
        .default_compact_model
        .clone();

    // Trigger MQTT retained republish so Runtimes pick up the new value
    // immediately (no need to wait for the next periodic publish).
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    tracing::info!(
        new = ?current,
        prev = ?prev,
        "default_compact_model updated via HTTP"
    );

    Ok(Json(DefaultCompactModelResponse {
        default_compact_model: current,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_deserialization_with_value() {
        let json = r#"{"default_compact_model":{"provider_id":"ollama","model_id":"qwen2.5:0.5b"}}"#;
        let req: PutDefaultCompactModelRequest = serde_json::from_str(json).unwrap();
        let dcm = req.default_compact_model.unwrap();
        assert_eq!(dcm.provider_id, "ollama");
        assert_eq!(dcm.model_id, "qwen2.5:0.5b");
    }

    #[test]
    fn request_deserialization_with_null_clears() {
        let json = r#"{"default_compact_model":null}"#;
        let req: PutDefaultCompactModelRequest = serde_json::from_str(json).unwrap();
        assert!(req.default_compact_model.is_none());
    }

    #[test]
    fn request_deserialization_missing_field_is_none() {
        // Omitted field → deserializes to None via #[serde(default)].
        let json = r#"{}"#;
        let req: PutDefaultCompactModelRequest = serde_json::from_str(json).unwrap();
        assert!(req.default_compact_model.is_none());
    }
}
