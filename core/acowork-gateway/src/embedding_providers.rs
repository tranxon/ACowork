//! Cloud embedding provider management (catalog + selection + vault keys).
//!
//! Independent from the LLM provider pipeline (`provider_api.rs`) and the
//! local ONNX embedding model pipeline (`embedding_api.rs`). This module
//! handles cloud embedding providers such as 字节火山方舟 / 阿里百炼 /
//! 硅基流动, all of which expose an OpenAI-compatible `/v1/embeddings`
//! endpoint.
//!
//! ## Architecture
//!
//! - **Catalog** (`assets/offline_embedding_providers.json`): read-only,
//!   bundled with the binary. Mirrors the structure of
//!   `offline_providers.json` for LLM providers but with embedding-specific
//!   fields (`dimensions`, `context_length`, `embedding_modalities`).
//!   Loaded once at startup into `ResourceCache.embedding_providers`.
//!
//! - **Active selection** (`data_dir/active_embedding_provider.json`):
//!   user-writable, persisted across restarts. Tracks which provider +
//!   model the user has chosen as the active cloud embedding target.
//!
//! - **API keys** (Vault): stored under `_embedding_{provider_id}` namespace,
//!   keeping them separate from LLM provider keys (`provider/{id}`) and
//!   search keys (`_search_{id}`).
//!
//! ## Endpoints
//!
//! - GET    /api/embedding-providers                          — list catalog + active
//! - POST   /api/embedding-providers/{id}/select              — select a model
//! - POST   /api/embedding-providers/{id}/api-key             — store API key
//! - DELETE /api/embedding-providers/{id}/api-key             — remove API key
//! - POST   /api/embedding-providers/{id}/test                — test connection

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::GatewayError;
use crate::http::routes::{ApiError, AppState};
use crate::vault::VaultFacade;
use acowork_core::protocol::{
    ActiveEmbeddingProvider, EmbeddingProviderCatalog, EmbeddingProviderDef,
    EmbeddingProviderModelDef, UserEmbeddingProviderEntry, UserEmbeddingProviderList,
};

// ── Catalog loader (read-only, bundled with binary) ─────────────────────

/// Load the cloud embedding provider catalog from disk.
///
/// Search order (mirrors `models_api.rs::offline_providers`):
///   1. `$CARGO_MANIFEST_DIR/../../assets/offline_embedding_providers.json` (dev)
///   2. `{exe_dir}/offline_embedding_providers.json`                       (installer)
///   3. `{cwd}/offline_embedding_providers.json`                           (dev convenience)
///
/// Returns an empty catalog if no file is found anywhere — never panics,
/// so the Gateway can still boot even with a broken install.
pub fn load_embedding_provider_catalog() -> EmbeddingProviderCatalog {
    static CATALOG: OnceLock<EmbeddingProviderCatalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            match load_catalog_from_disk() {
                Ok(cat) => {
                    tracing::info!(
                        providers = cat.providers.len(),
                        "Loaded embedding provider catalog"
                    );
                    cat
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to load embedding provider catalog, using empty"
                    );
                    EmbeddingProviderCatalog::default()
                }
            }
        })
        .clone()
}

fn load_catalog_from_disk() -> Result<EmbeddingProviderCatalog, String> {
    for path in build_candidates() {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("parse {}: {e}", path.display()))?;
        return catalog_from_json(value);
    }
    Err("offline_embedding_providers.json not found in any candidate path".into())
}

fn build_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // 1. Dev / test via cargo
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = PathBuf::from(manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("offline_embedding_providers.json");
        candidates.push(p);
    }

    // 2. Next to the executable (installer-provided)
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("offline_embedding_providers.json"));
    }

    // 3. Current working directory (dev convenience)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("offline_embedding_providers.json"));
    }

    candidates
}

/// Parse the JSON catalog into the typed struct.
///
/// The JSON shape matches the existing offline_providers.json "outer map"
/// convention: top-level keys are provider ids.
fn catalog_from_json(value: Value) -> Result<EmbeddingProviderCatalog, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "root must be a JSON object".to_string())?;

    let mut providers = HashMap::new();
    for (id, raw) in obj {
        let def: EmbeddingProviderDef = serde_json::from_value(raw.clone())
            .map_err(|e| format!("provider '{id}': {e}"))?;
        // Cross-check: def.id must match the outer key
        if def.id != *id {
            return Err(format!(
                "provider outer key '{id}' does not match inner id '{}'",
                def.id
            ));
        }
        // Validate every model has dimensions > 0
        for (model_id, model) in &def.models {
            if model.dimensions == 0 {
                return Err(format!(
                    "provider '{id}' model '{model_id}': dimensions must be > 0"
                ));
            }
        }
        providers.insert(id.clone(), def);
    }

    Ok(EmbeddingProviderCatalog {
        version: 1,
        providers,
    })
}

// ── Active selection persistence ────────────────────────────────────────

fn active_selection_path(data_dir: &Path) -> PathBuf {
    data_dir.join("active_embedding_provider.json")
}

/// Load the active cloud embedding selection from disk.
/// Returns `None` when the file is absent (user has not picked a cloud
/// provider yet — Runtime continues to use the local ONNX service).
pub fn load_active_embedding_provider(data_dir: &Path) -> Option<ActiveEmbeddingProvider> {
    let path = active_selection_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<ActiveEmbeddingProvider>(&raw) {
            Ok(active) => Some(active),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse active_embedding_provider.json, ignoring"
                );
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read active_embedding_provider.json, ignoring"
            );
            None
        }
    }
}

/// Persist the active selection to disk. Atomic write via temp + rename.
pub fn save_active_embedding_provider(
    data_dir: &Path,
    active: &ActiveEmbeddingProvider,
) -> Result<(), GatewayError> {
    std::fs::create_dir_all(data_dir).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to create data dir '{}': {e}",
            data_dir.display()
        ))
    })?;
    let final_path = active_selection_path(data_dir);
    let tmp_path = final_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(active).map_err(|e| {
        GatewayError::Config(format!("Failed to serialise active embedding provider: {e}"))
    })?;
    std::fs::write(&tmp_path, json).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to write '{}': {e}",
            tmp_path.display()
        ))
    })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to rename '{}' -> '{}': {e}",
            tmp_path.display(),
            final_path.display()
        ))
    })?;
    Ok(())
}

/// Clear the active selection by deleting the file. Idempotent.
pub fn clear_active_embedding_provider(data_dir: &Path) -> Result<(), GatewayError> {
    let path = active_selection_path(data_dir);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| {
            GatewayError::Config(format!(
                "Failed to delete '{}': {e}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

// ── User-added provider list ───────────────────────────────────────────
//
// User-added (custom) embedding providers are persisted separately from
// the bundled `offline_embedding_providers.json`. The offline file is
// immutable (read-only, shipped with the binary); the user list is the
// runtime-writable extension point. The list handler merges the two at
// request time — user entries with the same id win (so the user can
// override a bundled provider's `api` if it moves).

fn user_provider_list_path(data_dir: &Path) -> PathBuf {
    data_dir.join("user_embedding_providers.json")
}

/// Load the user-added provider list from disk. Returns an empty list
/// (not an error) when the file is missing — this is the common case
/// before the user adds any custom provider.
pub fn load_user_embedding_providers(data_dir: &Path) -> UserEmbeddingProviderList {
    let path = user_provider_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<UserEmbeddingProviderList>(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse user_embedding_providers.json, ignoring"
                );
                UserEmbeddingProviderList::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserEmbeddingProviderList::default(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read user_embedding_providers.json, ignoring"
            );
            UserEmbeddingProviderList::default()
        }
    }
}

/// Persist the user-added provider list to disk (atomic via tmp+rename).
pub fn save_user_embedding_providers(
    data_dir: &Path,
    list: &UserEmbeddingProviderList,
) -> Result<(), GatewayError> {
    std::fs::create_dir_all(data_dir).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to create data dir '{}': {e}",
            data_dir.display()
        ))
    })?;
    let final_path = user_provider_list_path(data_dir);
    let tmp_path = final_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(list).map_err(|e| {
        GatewayError::Config(format!("Failed to serialise user embedding providers: {e}"))
    })?;
    std::fs::write(&tmp_path, json).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to write '{}': {e}",
            tmp_path.display()
        ))
    })?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to rename '{}' -> '{}': {e}",
            tmp_path.display(),
            final_path.display()
        ))
    })?;
    Ok(())
}

/// Merge the offline catalog with the user-added list.
///
/// Order:
/// 1. All offline entries are inserted first (`def.custom = false`).
/// 2. User entries overwrite same-id offline entries (user wins,
///    `def.custom = true`). This lets the user override a bundled
///    provider's `api` when it moves.
/// 3. New user entries are appended.
pub fn merged_embedding_catalog(
    data_dir: &Path,
) -> std::collections::HashMap<String, EmbeddingProviderDef> {
    let offline = load_embedding_provider_catalog();
    let user = load_user_embedding_providers(data_dir);

    let mut out: std::collections::HashMap<String, EmbeddingProviderDef> =
        std::collections::HashMap::with_capacity(offline.providers.len() + user.providers.len());
    for (id, mut def) in offline.providers {
        def.custom = false;
        out.insert(id, def);
    }
    for entry in user.providers {
        let def = EmbeddingProviderDef {
            id: entry.id.clone(),
            name: entry.name,
            api: entry.api,
            protocol: "openai-compatible".to_string(),
            env: Vec::new(),
            doc: None,
            models: entry.models,
            custom: true,
        };
        out.insert(entry.id, def);
    }
    out
}

// ── Broker helpers ──────────────────────────────────────────────────────

/// Snapshot of the active cloud embedding selection for the MQTT broker.
///
/// All three fields are empty strings when no cloud selection exists
/// (Runtime should fall back to local ONNX in that case). When
/// `active_provider_id` is non-empty, Runtime should construct a
/// `RemoteEmbeddingProvider` against `active_base_url` and authenticate
/// with `active_api_key`.
#[derive(Debug, Clone, Default)]
pub struct ActiveCloudEmbeddingSnapshot {
    pub active_provider_id: String,
    pub active_base_url: String,
    pub active_api_key: String,
}

impl ActiveCloudEmbeddingSnapshot {
    pub fn empty() -> Self {
        Self {
            active_provider_id: String::new(),
            active_base_url: String::new(),
            active_api_key: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active_provider_id.is_empty()
    }
}

/// Resolve the active cloud embedding selection + decrypted API key.
///
/// Reads `active_embedding_provider.json` from `data_dir`, then looks up
/// the Vault-stored API key for the selected provider. Returns an empty
/// snapshot when:
/// - no selection file exists, OR
/// - the selection file is malformed, OR
/// - the provider has no API key in the Vault, OR
/// - the Vault itself is unavailable.
///
/// In all "empty" cases the caller (MQTT broker / builder) should treat
/// it as "no cloud provider active" and let Runtime continue with the
/// local ONNX service.
pub fn resolve_active_cloud_embedding(
    data_dir: &Path,
    vault: &VaultFacade,
) -> ActiveCloudEmbeddingSnapshot {
    let Some(active) = load_active_embedding_provider(data_dir) else {
        return ActiveCloudEmbeddingSnapshot::empty();
    };
    if active.provider_id.is_empty() {
        return ActiveCloudEmbeddingSnapshot::empty();
    }
    let api_key = match vault.get_embedding_key(&active.provider_id) {
        Ok(k) if !k.is_empty() => k,
        Ok(_) => {
            tracing::warn!(
                provider_id = %active.provider_id,
                "Active cloud embedding provider has an empty API key in the Vault; \
                 falling back to local ONNX"
            );
            return ActiveCloudEmbeddingSnapshot::empty();
        }
        Err(e) => {
            tracing::warn!(
                provider_id = %active.provider_id,
                error = %e,
                "Failed to decrypt active embedding provider key from the Vault; \
                 falling back to local ONNX"
            );
            return ActiveCloudEmbeddingSnapshot::empty();
        }
    };
    ActiveCloudEmbeddingSnapshot {
        active_provider_id: active.provider_id,
        active_base_url: active.base_url,
        active_api_key: api_key,
    }
}

// ── REST types ──────────────────────────────────────────────────────────

/// GET /api/embedding-providers response.
#[derive(Debug, Serialize)]
pub struct EmbeddingProvidersResponse {
    /// All known cloud embedding providers (catalog)
    pub providers: Vec<EmbeddingProviderView>,
    /// Currently-active cloud selection, if any.
    /// `None` means Runtime uses the local ONNX service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<ActiveEmbeddingProviderView>,
}

/// Per-provider view in the list endpoint.
#[derive(Debug, Serialize)]
pub struct EmbeddingProviderView {
    pub id: String,
    pub name: String,
    pub api: String,
    pub protocol: String,
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub models: Vec<EmbeddingProviderModelView>,
    /// Masked preview of the stored API key, or `None` if no key configured.
    /// Use the explicit endpoints below to write / delete the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_preview: Option<String>,
    /// Whether an API key is currently stored in Vault.
    pub has_api_key: bool,
    /// True for user-added providers (persisted in `user_embedding_providers.json`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingProviderModelView {
    pub id: String,
    pub name: String,
    pub dimensions: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    pub embedding_modalities: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ActiveEmbeddingProviderView {
    pub provider_id: String,
    pub model_id: String,
    pub dimension: u32,
    pub base_url: String,
    pub has_api_key: bool,
    pub selected_at: String,
}

impl From<ActiveEmbeddingProvider> for ActiveEmbeddingProviderView {
    fn from(a: ActiveEmbeddingProvider) -> Self {
        Self {
            provider_id: a.provider_id,
            model_id: a.model_id,
            dimension: a.dimension,
            base_url: a.base_url,
            has_api_key: a.has_api_key,
            selected_at: a.selected_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SelectEmbeddingModelRequest {
    /// Model id within the provider (must match a model in the catalog)
    pub model_id: String,
}

#[derive(Debug, Serialize)]
pub struct SelectEmbeddingModelResponse {
    pub provider_id: String,
    pub model_id: String,
    pub dimension: u32,
    pub base_url: String,
    pub has_api_key: bool,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct SetEmbeddingKeyRequest {
    /// API key to encrypt and store in Vault
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingKeyResponse {
    pub provider_id: String,
    pub has_api_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_preview: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingTestResponse {
    pub provider_id: String,
    pub model_id: String,
    pub ok: bool,
    pub dimension: Option<u32>,
    pub message: String,
}

/// POST /api/embedding-providers — add a user-defined cloud embedding provider.
///
/// Persists to `data_dir/user_embedding_providers.json`. The `id` MUST be
/// unique and MUST NOT collide with a bundled catalog id (use PUT on
/// `/api/embedding-providers/{id}/api-key` for bundled providers).
#[derive(Debug, Deserialize)]
pub struct AddEmbeddingProviderRequest {
    /// Stable provider id (e.g. "custom-my-proxy").
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// OpenAI-compatible base URL (e.g. "https://api.example.com/v1").
    pub api: String,
    /// Models offered by this provider, keyed by model id. Each model
    /// MUST have `dimensions > 0`. The map key MUST match each model's
    /// inner `id` field (enforced in the handler).
    pub models: std::collections::HashMap<String, EmbeddingProviderModelDef>,
    /// Optional API key to store in Vault at the same time. If absent
    /// or empty, the user must POST to `/api/embedding-providers/{id}/api-key`
    /// later before this provider can be used.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// PUT /api/embedding-providers/{id} — update a user-added cloud embedding provider.
///
/// All fields are optional; only the ones present in the request body
/// are applied. Refuses to update a bundled (offline) provider — those
/// are read-only.
#[derive(Debug, Deserialize)]
pub struct UpdateEmbeddingProviderRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New OpenAI-compatible base URL.
    #[serde(default)]
    pub api: Option<String>,
    /// New model set (replaces the entire set if present).
    #[serde(default)]
    pub models: Option<std::collections::HashMap<String, EmbeddingProviderModelDef>>,
}

/// Generic response for add / update / delete embedding provider.
#[derive(Debug, Serialize)]
pub struct EmbeddingProviderResponse {
    pub id: String,
    pub name: String,
    pub api: String,
    pub custom: bool,
    pub models: Vec<String>,
    pub message: String,
}

// ── Router ──────────────────────────────────────────────────────────────

/// Build the `/api/embedding-providers/*` router.
pub fn embedding_providers_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/embedding-providers",
            get(list_embedding_providers).post(add_embedding_provider),
        )
        .route(
            "/api/embedding-providers/{id}",
            put(update_embedding_provider).delete(delete_embedding_provider),
        )
        .route(
            "/api/embedding-providers/{id}/select",
            post(select_embedding_model),
        )
        .route(
            "/api/embedding-providers/{id}/api-key",
            post(set_embedding_api_key).delete(delete_embedding_api_key),
        )
        .route(
            "/api/embedding-providers/{id}/test",
            post(test_embedding_provider),
        )
}

// ── Handlers ────────────────────────────────────────────────────────────

/// GET /api/embedding-providers
///
/// Returns the full catalog (offline + user-merged) plus the active
/// selection (if any). Vault keys are reported only as masked previews.
pub async fn list_embedding_providers(State(state): State<AppState>) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;

    // Active selection lives in data_dir (via config)
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| c.data_dir.clone())
        .unwrap_or_default();
    drop(gw);

    let active = if data_dir.is_empty() {
        None
    } else {
        load_active_embedding_provider(Path::new(&data_dir))
    };
    let active_view = active.map(ActiveEmbeddingProviderView::from);

    let catalog = merged_embedding_catalog(Path::new(&data_dir));

    let gw = state.gateway_state.read().await;
    let providers: Vec<EmbeddingProviderView> = catalog
        .values()
        .map(|def| {
            let has_api_key = gw.vault.has_embedding_key(&def.id);
            let key_preview = if has_api_key {
                gw.vault
                    .list_embedding_keys()
                    .ok()
                    .and_then(|keys| {
                        keys.iter()
                            .find(|k| k.provider == def.id)
                            .map(|k| k.key_preview.clone())
                    })
            } else {
                None
            };
            EmbeddingProviderView {
                id: def.id.clone(),
                name: def.name.clone(),
                api: def.api.clone(),
                protocol: def.protocol.clone(),
                env: def.env.clone(),
                doc: def.doc.clone(),
                models: def
                    .models
                    .values()
                    .map(|m| EmbeddingProviderModelView {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        dimensions: m.dimensions,
                        context_length: m.context_length,
                        embedding_modalities: m.embedding_modalities.clone(),
                    })
                    .collect(),
                key_preview,
                has_api_key,
                custom: def.custom,
            }
        })
        .collect();
    drop(gw);

    Json(EmbeddingProvidersResponse {
        providers,
        active: active_view,
    })
}

/// POST /api/embedding-providers/{id}/select
///
/// Validates the provider + model exist in the catalog, then persists
/// the selection. Does NOT touch Vault — the API key may be set later
/// or already be present.
pub async fn select_embedding_model(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
    Json(req): Json<SelectEmbeddingModelRequest>,
) -> Response {
    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => c.data_dir.clone(),
        None => {
            return ApiError::internal("Gateway config not initialized").into_response();
        }
    };
    let has_api_key = gw.vault.has_embedding_key(&id);
    drop(gw);

    let catalog = merged_embedding_catalog(Path::new(&data_dir));
    let provider = match catalog.get(&id) {
        Some(p) => p,
        None => {
            return ApiError::not_found(&format!("Unknown embedding provider '{id}'"))
                .into_response();
        }
    };
    let model = match provider.models.get(&req.model_id) {
        Some(m) => m,
        None => {
            return ApiError::not_found(&format!(
                "Unknown model '{}' for provider '{id}'",
                req.model_id
            ))
            .into_response();
        }
    };

    let active = ActiveEmbeddingProvider {
        provider_id: id.clone(),
        model_id: req.model_id.clone(),
        dimension: model.dimensions,
        base_url: provider.api.clone(),
        has_api_key,
        selected_at: Utc::now().to_rfc3339(),
    };

    if let Err(e) = save_active_embedding_provider(std::path::Path::new(&data_dir), &active) {
        return ApiError::internal(&format!(
            "Failed to persist active embedding provider: {e}"
        ))
        .into_response();
    }

    // S1-5b trigger: re-publish `acowork/global/embedding_models` so Runtime
    // picks up the new active_provider_id / active_api_key on the next
    // config refresh (or immediately, if Runtime is online). The publisher
    // loop calls `build_available_embedding_models` which reads the
    // selection from disk + decrypts the key from Vault.
    if let Some(trigger) = state.mqtt_publisher_trigger.as_ref() {
        trigger.trigger();
    } else {
        tracing::debug!(
            "mqtt_publisher_trigger not installed; Runtime will pick up the new \
             active cloud embedding provider on its next retained-topic refresh"
        );
    }

    Json(SelectEmbeddingModelResponse {
        provider_id: id,
        model_id: req.model_id,
        dimension: model.dimensions,
        base_url: provider.api.clone(),
        has_api_key,
        status: "selected".into(),
        message: format!(
            "Selected cloud embedding model. Note: the embedding service will pick up the \
             new provider on its next config refresh; existing memory stores with a different \
             dimension ({model_dim}) will need migration via /api/embedding-models/start-migration.",
            model_dim = model.dimensions
        ),
    })
    .into_response()
}

/// POST /api/embedding-providers/{id}/api-key
///
/// Encrypts and stores the API key in Vault under `_embedding_{id}`.
/// Also refreshes `has_api_key` on the active selection (if it matches).
pub async fn set_embedding_api_key(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
    Json(req): Json<SetEmbeddingKeyRequest>,
) -> Response {
    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => c.data_dir.clone(),
        None => return ApiError::internal("Gateway config not initialized").into_response(),
    };
    drop(gw);

    let catalog = merged_embedding_catalog(Path::new(&data_dir));
    if !catalog.contains_key(&id) {
        return ApiError::not_found(&format!("Unknown embedding provider '{id}'"))
            .into_response();
    }
    if req.api_key.trim().is_empty() {
        return ApiError::bad_request("api_key must not be empty").into_response();
    }

    let mut gw = state.gateway_state.write().await;
    if let Err(e) = gw.vault.store_embedding_key(&id, &req.api_key) {
        return ApiError::internal(&format!("Failed to store embedding key: {e}")).into_response();
    }

    let data_dir = PathBuf::from(&data_dir);
    drop(gw);

    if let Some(mut active) = load_active_embedding_provider(&data_dir)
        && active.provider_id == id
    {
        active.has_api_key = true;
        if let Err(e) = save_active_embedding_provider(&data_dir, &active) {
            tracing::warn!(
                error = %e,
                "Failed to refresh has_api_key on active embedding provider"
            );
        }
    }

    let preview = preview_api_key(&req.api_key);
    Json(EmbeddingKeyResponse {
        provider_id: id,
        has_api_key: true,
        key_preview: Some(preview),
    })
    .into_response()
}

/// DELETE /api/embedding-providers/{id}/api-key
pub async fn delete_embedding_api_key(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
) -> Response {
    let mut gw = state.gateway_state.write().await;
    if let Err(e) = gw.vault.remove_embedding_key(&id) {
        return ApiError::internal(&format!("Failed to remove embedding key: {e}")).into_response();
    }

    let data_dir = match gw.config.as_ref() {
        Some(c) => PathBuf::from(&c.data_dir),
        None => return ApiError::internal("Gateway config not initialized").into_response(),
    };
    drop(gw);

    if let Some(mut active) = load_active_embedding_provider(&data_dir)
        && active.provider_id == id
    {
        active.has_api_key = false;
        if let Err(e) = save_active_embedding_provider(&data_dir, &active) {
            tracing::warn!(
                error = %e,
                "Failed to refresh has_api_key on active embedding provider"
            );
        }
    }

    Json(EmbeddingKeyResponse {
        provider_id: id,
        has_api_key: false,
        key_preview: None,
    })
    .into_response()
}

/// POST /api/embedding-providers/{id}/test
///
/// Sends a single short embedding request to verify connectivity +
/// credentials + dimension. Does NOT mutate any persisted state.
pub async fn test_embedding_provider(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
) -> Response {
    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => PathBuf::from(&c.data_dir),
        None => {
            return ApiError::internal("Gateway config not initialized").into_response();
        }
    };
    let api_key = gw.vault.get_embedding_key(&id).ok();
    drop(gw);

    let catalog = merged_embedding_catalog(&data_dir);
    let provider = match catalog.get(&id) {
        Some(p) => p,
        None => {
            return ApiError::not_found(&format!("Unknown embedding provider '{id}'"))
                .into_response();
        }
    };

    // Determine which model to test against:
    //   - if there is an active selection matching this provider, use that model
    //   - otherwise, use the first model in the catalog
    let (model_id, expected_dim) = match load_active_embedding_provider(&data_dir) {
        Some(active) if active.provider_id == id => {
            (active.model_id.clone(), active.dimension)
        }
        _ => {
            let first = match provider.models.values().next() {
                Some(m) => m,
                None => {
                    return ApiError::internal(&format!("Provider '{id}' has no models"))
                        .into_response();
                }
            };
            (first.id.clone(), first.dimensions)
        }
    };

    let key = match api_key {
        Some(k) => k,
        None => {
            return ApiError::bad_request(&format!(
                "No API key configured for embedding provider '{id}'. \
                 POST one via /api/embedding-providers/{id}/api-key first."
            ))
            .into_response();
        }
    };

    let url = format!("{}/embeddings", provider.api.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model_id,
        "input": "ping",
    });

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ApiError::internal(&format!("Failed to build HTTP client: {e}")).into_response();
        }
    };

    let resp = match client.post(&url).bearer_auth(&key).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            return ApiError::internal(&format!("Request to {url} failed: {e}")).into_response();
        }
    };

    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return ApiError::internal(&format!("Failed to read response body: {e}")).into_response();
        }
    };

    if !status.is_success() {
        return Json(EmbeddingTestResponse {
            provider_id: id,
            model_id,
            ok: false,
            dimension: None,
            message: format!("HTTP {}: {}", status.as_u16(), truncate(&text, 200)),
        })
        .into_response();
    }

    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return ApiError::internal(&format!("Invalid JSON response: {e}")).into_response();
        }
    };

    // Extract dimension from response: data[0].embedding.length
    let dim = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("embedding"))
        .and_then(|e| e.as_array())
        .map(|arr| arr.len() as u32);

    let dim_matches = dim == Some(expected_dim);
    Json(EmbeddingTestResponse {
        provider_id: id,
        model_id,
        ok: dim_matches,
        dimension: dim,
        message: if dim_matches {
            format!("OK — returned {dim} dims as expected.", dim = dim.unwrap_or(0))
        } else {
            format!(
                "Dimension mismatch: expected {expected_dim}, got {actual:?}",
                actual = dim
            )
        },
    })
    .into_response()
}

// ── User-defined provider CRUD ─────────────────────────────────────────

/// POST /api/embedding-providers — add a user-defined cloud embedding provider.
///
/// The request body MUST satisfy:
/// - `id` non-empty AND unique (no clash with bundled catalog entries
///   or already-saved user entries)
/// - `name` non-empty
/// - `api` non-empty AND `http(s)://`-prefixed
/// - `models` non-empty, every model `dimensions > 0`, and each map key
///   matches the inner `id` field
/// - `api_key` (optional) — if present and non-empty, stored in Vault
///
/// On success: persists the user list, optionally writes the Vault key,
/// and triggers MQTT republish so the broker picks up the new provider.
pub async fn add_embedding_provider(
    State(state): State<AppState>,
    Json(req): Json<AddEmbeddingProviderRequest>,
) -> Response {
    // ── Field validation ─────────────────────────────────────────────
    if req.id.trim().is_empty() {
        return ApiError::bad_request("id must not be empty").into_response();
    }
    if req.name.trim().is_empty() {
        return ApiError::bad_request("name must not be empty").into_response();
    }
    if req.api.trim().is_empty()
        || !(req.api.starts_with("http://") || req.api.starts_with("https://"))
    {
        return ApiError::bad_request("api must be a non-empty http(s) URL").into_response();
    }
    if req.models.is_empty() {
        return ApiError::bad_request("models must contain at least one entry").into_response();
    }
    for (map_key, model) in &req.models {
        if model.id != *map_key {
            return ApiError::bad_request(&format!(
                "model map key '{map_key}' does not match inner id '{}'",
                model.id
            ))
            .into_response();
        }
        if model.dimensions == 0 {
            return ApiError::bad_request(&format!(
                "model '{map_key}' must have dimensions > 0"
            ))
            .into_response();
        }
    }

    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => c.data_dir.clone(),
        None => return ApiError::internal("Gateway config not initialized").into_response(),
    };
    drop(gw);

    // Reject collisions with bundled catalog ids — bundled providers
    // are read-only here; users wanting to override them should edit
    // the bundled data, not layer a duplicate.
    let offline = load_embedding_provider_catalog();
    if offline.providers.contains_key(&req.id) {
        return ApiError::conflict(&format!(
            "Provider id '{}' is reserved by a bundled catalog entry; \
             choose a different id (e.g. 'custom-{req_id}')",
            req.id, req_id = req.id
        ))
        .into_response();
    }

    let mut list = load_user_embedding_providers(Path::new(&data_dir));
    if list.providers.iter().any(|p| p.id == req.id) {
        return ApiError::conflict(&format!(
            "User-added provider '{}' already exists",
            req.id
        ))
        .into_response();
    }

    // ── Persist user list ───────────────────────────────────────────
    list.providers.push(UserEmbeddingProviderEntry {
        id: req.id.clone(),
        name: req.name.clone(),
        api: req.api.clone(),
        models: req.models.clone(),
    });
    list.version = list.version.wrapping_add(1);

    if let Err(e) = save_user_embedding_providers(Path::new(&data_dir), &list) {
        return ApiError::internal(&format!(
            "Failed to persist user_embedding_providers.json: {e}"
        ))
        .into_response();
    }

    // ── Optional API key ────────────────────────────────────────────
    let has_api_key = if let Some(ref key) = req.api_key
        && !key.trim().is_empty()
    {
        let mut gw = state.gateway_state.write().await;
        if let Err(e) = gw.vault.store_embedding_key(&req.id, key) {
            return ApiError::internal(&format!(
                "Failed to store embedding key for '{}': {e}",
                req.id
            ))
            .into_response();
        }
        true
    } else {
        false
    };

    // ── Trigger MQTT republish so the broker sees the new provider on
    //    next config refresh. Runtime reads the merged catalog via
    //    `acowork/global/embedding_models` retained topic.
    if let Some(trigger) = state.mqtt_publisher_trigger.as_ref() {
        trigger.trigger();
    }

    let _ = has_api_key; // currently unused in response, kept for future has_api_key surfacing

    Json(EmbeddingProviderResponse {
        id: req.id,
        name: req.name,
        api: req.api,
        custom: true,
        models: req.models.keys().cloned().collect(),
        message: "Custom cloud embedding provider added".into(),
    })
    .into_response()
}

/// PUT /api/embedding-providers/{id} — update a user-added cloud embedding provider.
///
/// All fields are optional; only the ones present in the request body
/// are applied. Refuses to update a bundled (offline) provider — those
/// are immutable here.
pub async fn update_embedding_provider(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
    Json(req): Json<UpdateEmbeddingProviderRequest>,
) -> Response {
    // ── Field validation ─────────────────────────────────────────────
    if let Some(ref url) = req.api
        && !url.trim().is_empty()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        return ApiError::bad_request("api must start with http:// or https://").into_response();
    }
    if let Some(ref name) = req.name
        && name.trim().is_empty()
    {
        return ApiError::bad_request("name must not be empty").into_response();
    }
    if let Some(ref models) = req.models {
        if models.is_empty() {
            return ApiError::bad_request("models must contain at least one entry")
                .into_response();
        }
        for (map_key, model) in models {
            if model.id != *map_key {
                return ApiError::bad_request(&format!(
                    "model map key '{map_key}' does not match inner id '{}'",
                    model.id
                ))
                .into_response();
            }
            if model.dimensions == 0 {
                return ApiError::bad_request(&format!(
                    "model '{map_key}' must have dimensions > 0"
                ))
                .into_response();
            }
        }
    }

    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => c.data_dir.clone(),
        None => return ApiError::internal("Gateway config not initialized").into_response(),
    };
    drop(gw);

    let mut list = load_user_embedding_providers(Path::new(&data_dir));
    let entry = match list.providers.iter_mut().find(|p| p.id == id) {
        Some(e) => e,
        None => {
            return ApiError::not_found(&format!(
                "User-added provider '{id}' not found; only user-added providers can be updated"
            ))
            .into_response();
        }
    };

    if let Some(ref name) = req.name {
        entry.name = name.clone();
    }
    if let Some(ref api) = req.api {
        entry.api = api.clone();
    }
    if let Some(ref models) = req.models {
        entry.models = models.clone();
    }
    let snapshot_name = entry.name.clone();
    let snapshot_api = entry.api.clone();
    let snapshot_models: Vec<String> = entry.models.keys().cloned().collect();

    list.version = list.version.wrapping_add(1);
    if let Err(e) = save_user_embedding_providers(Path::new(&data_dir), &list) {
        return ApiError::internal(&format!(
            "Failed to persist user_embedding_providers.json: {e}"
        ))
        .into_response();
    }

    // Trigger MQTT republish.
    if let Some(trigger) = state.mqtt_publisher_trigger.as_ref() {
        trigger.trigger();
    }

    Json(EmbeddingProviderResponse {
        id,
        name: snapshot_name,
        api: snapshot_api,
        custom: true,
        models: snapshot_models,
        message: "Custom cloud embedding provider updated".into(),
    })
    .into_response()
}

/// DELETE /api/embedding-providers/{id} — delete a user-added cloud embedding provider.
///
/// Refuses to delete a bundled (offline) provider. Also:
/// - removes the Vault-stored API key for this provider (if any)
/// - clears the active selection if it pointed at the deleted provider
pub async fn delete_embedding_provider(
    State(state): State<AppState>,
    AxumPath((id,)): AxumPath<(String,)>,
) -> Response {
    let gw = state.gateway_state.read().await;
    let data_dir = match gw.config.as_ref() {
        Some(c) => c.data_dir.clone(),
        None => return ApiError::internal("Gateway config not initialized").into_response(),
    };
    drop(gw);

    let mut list = load_user_embedding_providers(Path::new(&data_dir));
    let original_len = list.providers.len();
    list.providers.retain(|p| p.id != id);
    if list.providers.len() == original_len {
        return ApiError::not_found(&format!(
            "User-added provider '{id}' not found; only user-added providers can be deleted"
        ))
        .into_response();
    }
    list.version = list.version.wrapping_add(1);

    if let Err(e) = save_user_embedding_providers(Path::new(&data_dir), &list) {
        return ApiError::internal(&format!(
            "Failed to persist user_embedding_providers.json: {e}"
        ))
        .into_response();
    }

    // Best-effort cleanup of related state.
    {
        let mut gw = state.gateway_state.write().await;
        let _ = gw.vault.remove_embedding_key(&id);
    }
    if let Some(active) = load_active_embedding_provider(Path::new(&data_dir))
        && active.provider_id == id
        && let Err(e) = clear_active_embedding_provider(Path::new(&data_dir))
    {
        tracing::warn!(
            error = %e,
            provider_id = %id,
            "Failed to clear active embedding selection after deleting provider"
        );
    }

    // Trigger MQTT republish.
    if let Some(trigger) = state.mqtt_publisher_trigger.as_ref() {
        trigger.trigger();
    }

    Json(EmbeddingProviderResponse {
        id,
        name: String::new(),
        api: String::new(),
        custom: true,
        models: Vec::new(),
        message: "Custom cloud embedding provider deleted".into(),
    })
    .into_response()
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn preview_api_key(key: &str) -> String {
    if key.len() > 6 {
        format!("{}...{}", &key[..3], &key[key.len() - 3..])
    } else {
        "***".to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_from_json_accepts_valid_payload() {
        let json = serde_json::json!({
            "volcengine": {
                "id": "volcengine",
                "name": "字节火山方舟",
                "api": "https://ark.cn-beijing.volces.com/api/v3",
                "protocol": "openai-compatible",
                "env": ["ARK_API_KEY"],
                "models": {
                    "doubao-embedding": {
                        "id": "doubao-embedding",
                        "name": "Doubao Embedding",
                        "dimensions": 1024,
                        "context_length": 4096,
                        "embedding_modalities": ["text"]
                    }
                }
            }
        });
        let cat = catalog_from_json(json).expect("parse");
        assert_eq!(cat.providers.len(), 1);
        let p = &cat.providers["volcengine"];
        assert_eq!(p.api, "https://ark.cn-beijing.volces.com/api/v3");
        assert_eq!(p.protocol, "openai-compatible");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models["doubao-embedding"].dimensions, 1024);
    }

    #[test]
    fn catalog_from_json_rejects_zero_dimensions() {
        let json = serde_json::json!({
            "p": {
                "id": "p",
                "name": "P",
                "api": "https://example.com/v1",
                "models": {
                    "m": {
                        "id": "m",
                        "name": "M",
                        "dimensions": 0
                    }
                }
            }
        });
        assert!(catalog_from_json(json).is_err());
    }

    #[test]
    fn catalog_from_json_rejects_id_mismatch() {
        let json = serde_json::json!({
            "outer": {
                "id": "inner",
                "name": "X",
                "api": "https://x/v1",
                "models": {
                    "m": {"id": "m", "name": "M", "dimensions": 1}
                }
            }
        });
        assert!(catalog_from_json(json).is_err());
    }

    #[test]
    fn preview_api_key_masks_short_keys() {
        assert_eq!(preview_api_key(""), "***");
        assert_eq!(preview_api_key("abc"), "***");
        assert_eq!(preview_api_key("abcdef"), "***");
        assert_eq!(preview_api_key("abcdefghijk"), "abc...ijk");
    }
}
