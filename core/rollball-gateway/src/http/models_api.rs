//! Models API — provider model lists with offline fallback
//!
//! Endpoints:
//!   GET /api/models              — list all providers with models
//!   GET /api/models/{provider}   — get models for a specific provider
//!
//! Data sources (in priority order):
//!   1. In-memory cache (TTL 5 minutes)
//!   2. models.dev API (https://models.dev/api.json)
//!   3. External offline data file (loaded at runtime)
//!   4. Empty result

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

use crate::http::routes::AppState;

/// Cache TTL: 5 minutes
const CACHE_TTL_SECS: u64 = 300;

/// models.dev API base URL
const MODELS_DEV_URL: &str = "https://models.dev/api.json";



/// Providers that have both international and CN variants on models.dev
const CN_VARIANT_PROVIDERS: &[&str] = &["minimax", "zhipuai", "moonshotai", "alibaba"];

// ── Response types ────────────────────────────────────────────────────

/// A single model from models.dev
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub tool_call: Option<bool>,
    #[serde(default)]
    pub attachment: Option<bool>,
    #[serde(default)]
    pub temperature: Option<bool>,
    #[serde(default)]
    pub release_date: Option<String>,
    /// Context window size (total tokens: input + output)
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Maximum output tokens
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Maximum input tokens (from models.dev limit.input)
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    /// Knowledge cutoff date (e.g. "2025-04")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,
    /// Input cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost: Option<f64>,
    /// Output cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost: Option<f64>,
    /// Input modalities (e.g. ["text", "image"])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    /// Output modalities (e.g. ["text"])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
}

/// Provider info with its models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub id: String,
    pub name: String,
    pub models: Vec<ModelInfo>,
}

/// Cached response from models.dev
pub(crate) struct CachedData {
    /// JSON value from models.dev
    data: serde_json::Value,
    /// When the cache was populated
    fetched_at: std::time::Instant,
}

/// Shared cache state
pub(crate) type ModelsCache = Arc<RwLock<Option<CachedData>>>;

// ── Route builder ─────────────────────────────────────────────────────

pub fn models_routes() -> Router<AppState> {
    Router::new()
        .route("/api/models", get(list_all_providers))
        .route("/api/models/{provider}", get(get_provider_models))
}

/// Reset routes — Gateway lifecycle management
pub fn reset_routes() -> Router<AppState> {
    Router::new().route("/api/gateway/reset", post(reset_gateway))
}

// ── Offline data ──────────────────────────────────────────────────────

/// Global data directory path, set once during server startup.
/// Used as the primary search location for `offline_providers.json`
/// (writable, kept up-to-date by background network refresh).
static DATA_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Set the data directory for offline provider file search.
/// Must be called once before any request handling.
pub(crate) fn set_data_dir(dir: &std::path::Path) {
    let _ = DATA_DIR.set(dir.to_path_buf());
}

/// Load offline provider data from a file on disk.
///
/// Search order:
///   1. $CARGO_MANIFEST_DIR/../../assets/offline_providers.json  (dev / test via cargo)
///   2. {data_dir}/offline_providers.json                         (network-updated, writable)
///   3. {exe_dir}/offline_providers.json                          (installer-provided)
///   4. {cwd}/offline_providers.json                              (dev convenience)
///
/// Returns an empty JSON object if no file is found anywhere.
fn offline_providers() -> &'static serde_json::Value {
    static DATA: OnceLock<serde_json::Value> = OnceLock::new();
    DATA.get_or_init(|| {
        load_offline_providers_from_file()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
    })
}

fn load_offline_providers_from_file() -> Option<serde_json::Value> {
    let candidates = build_offline_file_candidates();

    for path in &candidates {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    match serde_json::from_str::<serde_json::Value>(&content) {
                        Ok(data) => {
                            tracing::info!("Loaded offline providers from: {}", path.display());
                            return Some(data);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to parse offline_providers.json at {}: {}",
                                path.display(),
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to read offline_providers.json at {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    tracing::warn!(
        "offline_providers.json not found in any candidate path, using empty data"
    );
    None
}

fn build_offline_file_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. CARGO_MANIFEST_DIR ../../assets/  (dev and test via cargo)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let assets = std::path::PathBuf::from(&manifest_dir)
            .join("..").join("..").join("assets").join("offline_providers.json");
        if assets.exists() {
            candidates.push(assets);
        }
    }

    // 1. Data directory (writable, kept up-to-date by background refresh)
    if let Some(data_dir) = DATA_DIR.get() {
        candidates.push(data_dir.join("offline_providers.json"));
    }

    // 2. Same directory as the executable (installer-provided, read-only)
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("offline_providers.json"));
    }

    // 3. Current working directory (dev convenience)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("offline_providers.json"));
    }

    candidates
}

// ── CN variant helpers ────────────────────────────────────────────────

/// Build the list of provider IDs to query for a given provider_id.
///
/// For providers with CN variants, both the base and `-cn` suffixed IDs
/// are returned. For zhipuai, the `zhipuai-coding-plan` variant is also
/// included. No legacy alias mapping (qwen→alibaba, etc.) is performed —
/// callers should use the canonical models.dev provider ID directly.
fn provider_ids_to_query(provider_id: &str) -> Vec<String> {
    let mut ids = vec![provider_id.to_string()];
    if CN_VARIANT_PROVIDERS.contains(&provider_id) {
        ids.push(format!("{}-cn", provider_id));
    }
    // zhipuai also has zhipuai-coding-plan variant
    if provider_id == "zhipuai" {
        ids.push("zhipuai-coding-plan".to_string());
    }
    ids
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Fetch models.dev data synchronously (blocking caller until network completes).
/// Returns Err only when both cache miss and network fetch fail.
async fn fetch_models(cache: &ModelsCache) -> Result<serde_json::Value, String> {
    // Check cache
    {
        let guard = cache.read().await;
        if let Some(ref cached) = *guard
            && cached.fetched_at.elapsed().as_secs() < CACHE_TTL_SECS
        {
            return Ok(cached.data.clone());
        }
    }

    // Fetch from models.dev
    let response = reqwest::get(MODELS_DEV_URL)
        .await
        .map_err(|e| format!("Failed to fetch models.dev: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("models.dev returned status {}", response.status()));
    }

    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse models.dev response: {}", e))?;

    // Update cache
    {
        let mut guard = cache.write().await;
        *guard = Some(CachedData {
            data: data.clone(),
            fetched_at: std::time::Instant::now(),
        });
    }

    Ok(data)
}

/// Fetch models.dev data in the background, updating the cache on success.
/// Returns immediately — does NOT wait for the network request.
/// On success the data is persisted to `{data_dir}/offline_providers.json`
/// so the full provider list stays available on next launch.
pub(crate) fn fetch_models_background(cache: ModelsCache) {
    tokio::spawn(async move {
        match fetch_models(&cache).await {
            Ok(data) => {
                // Persist to data_dir so next launch can use updated data.
                // Write to a temp file first, then rename — avoids readers
                // (e.g. reset_gateway) seeing a half-written file.
                if let Some(dir) = DATA_DIR.get() {
                    let path = dir.join("offline_providers.json");
                    let tmp = dir.join("offline_providers.json.tmp");
                    let content = serde_json::to_string_pretty(&data).unwrap_or_default();
                    if let Err(e) = std::fs::write(&tmp, &content) {
                        tracing::warn!("Failed to write offline providers tmp: {}", e);
                    } else if let Err(e) = std::fs::rename(&tmp, &path) {
                        tracing::warn!("Failed to rename offline providers tmp: {}", e);
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
                tracing::debug!("Background models.dev refresh succeeded");
            }
            Err(e) => tracing::warn!("Background models.dev refresh failed: {}", e),
        }
    });
}

/// Extract models from a provider JSON object.
/// Works with both models.dev API response and offline_providers.json format.
fn extract_models(provider_data: &serde_json::Value) -> Vec<ModelInfo> {
    let models_obj = match provider_data.get("models") {
        Some(serde_json::Value::Object(m)) => m,
        _ => return Vec::new(),
    };

    let mut models = Vec::new();
    for (id, model_data) in models_obj {
        let context_window = model_data
            .get("limit")
            .and_then(|v| v.get("context"))
            .and_then(|v| v.as_u64());

        let max_tokens = model_data
            .get("limit")
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_u64());

        let max_input_tokens = model_data
            .get("limit")
            .and_then(|v| v.get("input"))
            .and_then(|v| v.as_u64());

        let model = ModelInfo {
            id: id.clone(),
            name: model_data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string(),
            family: model_data
                .get("family")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            reasoning: model_data.get("reasoning").and_then(|v| v.as_bool()),
            tool_call: model_data.get("tool_call").and_then(|v| v.as_bool()),
            attachment: model_data.get("attachment").and_then(|v| v.as_bool()),
            temperature: model_data.get("temperature").and_then(|v| v.as_bool()),
            release_date: model_data
                .get("release_date")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            context_window,
            max_tokens,
            max_input_tokens,
            knowledge: model_data
                .get("knowledge")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            input_cost: model_data
                .get("cost")
                .and_then(|v| v.get("input"))
                .and_then(|v| v.as_f64()),
            output_cost: model_data
                .get("cost")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_f64()),
            input_modalities: model_data
                .get("modalities")
                .and_then(|v| v.get("input"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()),
            output_modalities: model_data
                .get("modalities")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()),
        };
        models.push(model);
    }

    // Sort: reasoning models first, then alphabetically by id
    models.sort_by(|a, b| {
        let a_reasoning = a.reasoning.unwrap_or(false);
        let b_reasoning = b.reasoning.unwrap_or(false);
        b_reasoning.cmp(&a_reasoning).then(a.id.cmp(&b.id))
    });

    models
}

/// Resolve provider data from the given JSON value, querying all variant IDs.
/// Returns (provider_name, models) or None if no data found.
fn resolve_provider(
    data: &serde_json::Value,
    provider_id: &str,
) -> Option<(String, Vec<ModelInfo>)> {
    let providers = data.as_object()?;
    let query_ids = provider_ids_to_query(provider_id);
    let mut all_models = Vec::new();
    let mut provider_name: Option<String> = None;

    for qid in &query_ids {
        if let Some(provider_data) = providers.get(qid) {
            if provider_name.is_none() {
                provider_name = Some(
                    provider_data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(provider_id)
                        .to_string(),
                );
            }
            let models = extract_models(provider_data);
            all_models.extend(models);
        }
    }

    if all_models.is_empty() && provider_name.is_none() {
        return None;
    }

    // Deduplicate by model id
    let mut seen = std::collections::HashSet::new();
    all_models.retain(|m| seen.insert(m.id.clone()));

    Some((provider_name.unwrap_or_else(|| provider_id.to_string()), all_models))
}

// ── Handlers ──────────────────────────────────────────────────────────

/// GET /api/models — list all providers with model counts
///
/// Response strategy: offline-first, background-refresh.
/// Returns offline provider data immediately (sub-ms), then spawns a background
/// task to fetch models.dev for subsequent requests. This avoids blocking the
/// UI (especially the onboarding page) on slow network calls.
async fn list_all_providers(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Always use offline data for the immediate response — it's always available
    // and contains the full provider list. The network refresh happens in background.
    let data = offline_providers().clone();

    // If cache is missing or stale, trigger a background refresh.
    // We do this AFTER cloning offline data so the response isn't blocked.
    {
        let guard = state.models_cache.read().await;
        let needs_refresh = match &*guard {
            None => true,
            Some(cached) => cached.fetched_at.elapsed().as_secs() >= CACHE_TTL_SECS,
        };
        if needs_refresh {
            fetch_models_background(state.models_cache.clone());
        }
    }

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Invalid provider data"})),
            ))
        }
    };

    let mut result = Vec::new();
    for (id, provider_data) in providers {
        let name = provider_data
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let model_count = provider_data
            .get("models")
            .and_then(|v| v.as_object())
            .map(|m| m.len())
            .unwrap_or(0);

        let mut entry = serde_json::json!({
            "id": id,
            "name": name,
            "model_count": model_count,
        });
        // Include the provider's base API URL when available (from models.dev or offline data)
        if let Some(api_url) = provider_data.get("api").and_then(|v| v.as_str()) {
            entry["api"] = serde_json::json!(api_url);
        }
        result.push(entry);
    }

    // Sort by id
    result.sort_by(|a, b| {
        a.get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
    });

    Ok(Json(serde_json::json!({"providers": result})))
}

/// GET /api/models/{provider} — get models for a specific provider
///
/// Resolution order:
///   1. Built-in offline data (instant, always available)
///   2. models.dev cache/API (fallback for providers not in offline data)
///   3. Empty result
///
/// For providers present in offline data, returns immediately and triggers a
/// background refresh of the models.dev cache. For custom providers not in
/// offline data, falls back to models.dev synchronously.
async fn get_provider_models(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // 1. Try offline data first (instant, no network)
    if let Some((name, models)) = resolve_provider(offline_providers(), &provider_id) {
        // Background refresh: if cache is stale, update it asynchronously
        {
            let guard = state.models_cache.read().await;
            let needs_refresh = match &*guard {
                None => true,
                Some(cached) => cached.fetched_at.elapsed().as_secs() >= CACHE_TTL_SECS,
            };
            if needs_refresh {
                fetch_models_background(state.models_cache.clone());
            }
        }
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models,
        }));
    }

    // 2. Try models.dev cache/API (for providers not in offline data)
    if let Ok(data) = fetch_models(&state.models_cache).await
        && let Some((name, models)) = resolve_provider(&data, &provider_id)
    {
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models,
        }));
    }

    // 3. Provider not found — return empty model list
    Ok(Json(ProviderModels {
        id: provider_id.clone(),
        name: provider_id,
        models: vec![],
    }))
}

/// POST /api/gateway/reset — reset Gateway state (reload models cache from disk)
///
/// When the frontend resets onboarding, this endpoint reloads the models.dev
/// cache from disk (if previously persisted) instead of re-fetching from the
/// network. If no disk cache exists (first use), triggers a background fetch.
async fn reset_gateway(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Try loading the latest data from data_dir/offline_providers.json
    let path = DATA_DIR.get().map(|d| d.join("offline_providers.json"));
    match path {
        Some(ref p) if p.exists() => {
            match std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            {
                Some(data) => {
                    let mut guard = state.models_cache.write().await;
                    *guard = Some(CachedData {
                        data,
                        fetched_at: std::time::Instant::now(),
                    });
                    tracing::info!("Gateway reset: reloaded from {}", p.display());
                    Ok(Json(serde_json::json!({"status": "ok", "source": "disk"})))
                }
                None => {
                    // File exists but failed to parse — fall back to background fetch
                    fetch_models_background(state.models_cache.clone());
                    Ok(Json(serde_json::json!({"status": "ok", "source": "network"})))
                }
            }
        }
        _ => {
            fetch_models_background(state.models_cache.clone());
            Ok(Json(serde_json::json!({"status": "ok", "source": "network"})))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

// ── Model capabilities lookup (used by IPC and vault hot-push) ──────────

/// Look up model capabilities for a specific provider + model_id.
/// Uses built-in offline provider data (always available, no network required).
/// Returns None if the model is not found in the offline data.
pub fn lookup_model_capabilities(
    provider: &str,
    model_id: &str,
) -> Option<rollball_core::protocol::ModelCapabilitiesInfo> {
    let data = offline_providers();
    lookup_model_capabilities_from_data(data, provider, model_id)
}

/// Look up model capabilities for a specific provider + model_id.
/// Tries the in-memory cache first (if data has been fetched from models.dev),
/// then falls back to built-in offline provider data.
/// Returns None if the model is not found in either source.
pub(crate) async fn lookup_model_capabilities_with_cache(
    cache: &ModelsCache,
    provider: &str,
    model_id: &str,
) -> Option<rollball_core::protocol::ModelCapabilitiesInfo> {
    // 1. Try in-memory cache (may have fresher data from models.dev)
    if let Ok(data) = fetch_models(cache).await
        && let Some(caps) = lookup_model_capabilities_from_data(&data, provider, model_id)
    {
        return Some(caps);
    }
    // 2. Fall back to offline data
    lookup_model_capabilities(provider, model_id)
}

/// Internal helper: look up model capabilities from a JSON data source.
fn lookup_model_capabilities_from_data(
    data: &serde_json::Value,
    provider: &str,
    model_id: &str,
) -> Option<rollball_core::protocol::ModelCapabilitiesInfo> {
    // 1. Try exact provider match first
    if let Some((_, models)) = resolve_provider(data, provider)
        && let Some(model) = models.iter().find(|m| m.id == model_id)
    {
        return model_to_capabilities(model);
    }
    // 2. Fallback: cross-provider search by model ID
    //    This handles cases like alibaba-cn proxying moonshotai/kimi-k2.6
    cross_provider_lookup(data, model_id)
}

/// Search all providers for a model matching the given model_id.
/// Strips provider prefix if present (e.g. "moonshotai/kimi-k2.6" -> "kimi-k2.6").
fn cross_provider_lookup(
    data: &serde_json::Value,
    model_id: &str,
) -> Option<rollball_core::protocol::ModelCapabilitiesInfo> {
    let bare_id = if model_id.contains('/') {
        model_id.split('/').next_back().unwrap_or(model_id)
    } else {
        model_id
    };

    let providers = data.as_object()?;
    for (pid, provider_data) in providers {
        if let Some(models) = extract_models(provider_data).into_iter().find(|m| m.id == bare_id || m.id == model_id) {
            tracing::debug!(
                model_id = model_id,
                found_in_provider = %pid,
                "Cross-provider model capabilities lookup succeeded"
            );
            return model_to_capabilities(&models);
        }
    }
    None
}

// ── Protocol type lookup (used by IPC to derive protocol from npm field) ──────

/// Derive protocol type from models.dev npm field.
///
/// Mapping rules:
/// - npm contains "anthropic" → ProtocolType::Anthropic
/// - npm contains "google"    → ProtocolType::Google
/// - npm contains "ollama"    → ProtocolType::Ollama
/// - everything else          → ProtocolType::OpenAI (default)
fn derive_protocol_type(npm: Option<&str>) -> rollball_core::protocol::ProtocolType {
    use rollball_core::protocol::ProtocolType;
    match npm {
        Some(s) if s.contains("anthropic") => ProtocolType::Anthropic,
        Some(s) if s.contains("google") => ProtocolType::Google,
        Some(s) if s.contains("ollama") => ProtocolType::Ollama,
        _ => ProtocolType::OpenAI,
    }
}

/// Look up protocol info for a provider+model combination using offline data.
///
/// Returns (protocol_type, api_override):
/// - protocol_type: derived from npm field (model-level > provider-level > default OpenAI)
/// - api_override: model-level api URL override if present
pub fn lookup_protocol_info(
    provider_id: &str,
    model_id: Option<&str>,
) -> (rollball_core::protocol::ProtocolType, Option<String>) {
    let data = offline_providers();
    lookup_protocol_info_from_data(data, provider_id, model_id)
}

/// Look up protocol info with cache freshness (tries network cache first).
/// Falls back to offline data when cache is unavailable.
pub(crate) async fn lookup_protocol_info_with_cache(
    cache: &ModelsCache,
    provider_id: &str,
    model_id: Option<&str>,
) -> (rollball_core::protocol::ProtocolType, Option<String>) {
    let data = match fetch_models(cache).await {
        Ok(d) => d,
        Err(_) => offline_providers().clone(),
    };
    lookup_protocol_info_from_data(&data, provider_id, model_id)
}

/// Internal helper: look up protocol info from a JSON data source.
///
/// Priority: model-level npm > provider-level npm > default OpenAI.
/// Model-level api override is returned when present in the model's provider block.
fn lookup_protocol_info_from_data(
    data: &serde_json::Value,
    provider_id: &str,
    model_id: Option<&str>,
) -> (rollball_core::protocol::ProtocolType, Option<String>) {
    use rollball_core::protocol::ProtocolType;

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => return (ProtocolType::OpenAI, None),
    };

    // Find the provider object
    let provider_obj = match providers.get(provider_id) {
        Some(p) => p,
        None => return (ProtocolType::OpenAI, None),
    };

    // Provider-level npm
    let provider_npm = provider_obj.get("npm").and_then(|v| v.as_str());

    // If model_id provided, check model-level override
    if let Some(mid) = model_id
        && let Some(models) = provider_obj.get("models").and_then(|m| m.as_object())
        && let Some(model_obj) = models.get(mid)
        && let Some(model_provider) = model_obj.get("provider").and_then(|p| p.as_object())
    {
        let model_npm = model_provider.get("npm").and_then(|v| v.as_str());
        let model_api = model_provider.get("api")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Model-level npm takes precedence
        if model_npm.is_some() {
            return (derive_protocol_type(model_npm), model_api);
        }

        // Model has provider block but no npm → use provider-level npm + model api
        if model_api.is_some() {
            return (derive_protocol_type(provider_npm), model_api);
        }
    }

    // Fall back to provider-level npm + provider-level api
    let provider_api = provider_obj.get("api")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (derive_protocol_type(provider_npm), provider_api)
}

/// Convert a ModelInfo to ModelCapabilitiesInfo
fn model_to_capabilities(model: &ModelInfo) -> Option<rollball_core::protocol::ModelCapabilitiesInfo> {
    Some(rollball_core::protocol::ModelCapabilitiesInfo {
        context_window: model.context_window.unwrap_or(0),
        max_output_tokens: model.max_tokens.unwrap_or(0),
        max_input_tokens: model.max_input_tokens,
        supports_tool_calling: model.tool_call.unwrap_or(true),
        supports_reasoning: model.reasoning,
        supports_attachment: model.attachment,
        supports_temperature: model.temperature,
        cost: match (model.input_cost, model.output_cost) {
            (Some(inp), Some(out)) => Some(rollball_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: Some(out),
            }),
            (Some(inp), None) => Some(rollball_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: None,
            }),
            (None, Some(out)) => Some(rollball_core::protocol::ModelCostInfo {
                input_per_million: None,
                output_per_million: Some(out),
            }),
            (None, None) => None,
        },
        modalities: match (&model.input_modalities, &model.output_modalities) {
            (Some(inp), Some(out)) => Some(rollball_core::protocol::ModelModalities {
                input: inp.clone(),
                output: out.clone(),
            }),
            _ => None,
        },
        name: Some(model.name.clone()),
        family: model.family.clone(),
        knowledge_cutoff: model.knowledge.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offline_providers_loads() {
        let data = offline_providers();
        assert!(data.is_object(), "offline_providers must be a JSON object");

        let obj = data.as_object().unwrap();
        // Verify all expected providers exist
        let expected = [
            "openai", "anthropic", "google", "deepseek", "minimax", "minimax-cn",
            "zhipuai", "moonshotai", "moonshotai-cn",
            "alibaba", "alibaba-cn", "groq", "mistral", "xai", "openrouter",
            "azure", "lmstudio",
        ];
        for pid in &expected {
            assert!(obj.contains_key(*pid), "Missing provider: {}", pid);
        }
    }

    #[test]
    fn test_offline_provider_has_required_fields() {
        let data = offline_providers();
        let openai = &data["openai"];
        assert!(openai.get("id").is_some(), "provider must have 'id'");
        assert!(openai.get("name").is_some(), "provider must have 'name'");
        assert!(openai.get("models").is_some(), "provider must have 'models'");

        // Check a model has required fields
        let models = openai["models"].as_object().unwrap();
        let (_, first_model) = models.iter().next().unwrap();
        assert!(first_model.get("id").is_some(), "model must have 'id'");
        assert!(first_model.get("name").is_some(), "model must have 'name'");
        assert!(first_model.get("family").is_some(), "model must have 'family'");
        assert!(first_model.get("reasoning").is_some(), "model must have 'reasoning'");
        assert!(first_model.get("attachment").is_some(), "model must have 'attachment'");
        assert!(first_model.get("tool_call").is_some(), "model must have 'tool_call'");
        assert!(first_model.get("limit").is_some(), "model must have 'limit'");
    }

    #[test]
    fn test_offline_provider_has_api_field_when_expected() {
        let data = offline_providers();
        // Providers that should have an api field in the source data
        let providers_with_api = [
            "deepseek", "minimax", "minimax-cn", "zhipuai",
            "moonshotai", "moonshotai-cn", "alibaba", "alibaba-cn",
            "openrouter", "lmstudio",
        ];
        for pid in &providers_with_api {
            let provider = &data[pid];
            assert!(
                provider.get("api").is_some(),
                "provider '{}' should have 'api' field",
                pid
            );
        }
    }

    #[test]
    fn test_provider_ids_to_query_simple() {
        assert_eq!(
            provider_ids_to_query("openai"),
            vec!["openai".to_string()]
        );
        assert_eq!(
            provider_ids_to_query("anthropic"),
            vec!["anthropic".to_string()]
        );
    }

    #[test]
    fn test_provider_ids_to_query_cn_variants() {
        assert_eq!(
            provider_ids_to_query("minimax"),
            vec!["minimax".to_string(), "minimax-cn".to_string()]
        );
        assert_eq!(
            provider_ids_to_query("alibaba"),
            vec!["alibaba".to_string(), "alibaba-cn".to_string()]
        );
    }

    #[test]
    fn test_provider_ids_to_query_zhipuai() {
        // zhipuai has both -cn and -coding-plan variants
        assert_eq!(
            provider_ids_to_query("zhipuai"),
            vec![
                "zhipuai".to_string(),
                "zhipuai-cn".to_string(),
                "zhipuai-coding-plan".to_string(),
            ]
        );
    }

    #[test]
    fn test_provider_ids_to_query_unknown() {
        // Unknown providers get no extra variants
        assert_eq!(
            provider_ids_to_query("some-new-provider"),
            vec!["some-new-provider".to_string()]
        );
    }

    #[test]
    fn test_extract_models() {
        let provider_data = serde_json::json!({
            "name": "Test Provider",
            "models": {
                "model-a": {
                    "name": "Model A",
                    "reasoning": true,
                    "tool_call": true,
                    "attachment": false,
                    "release_date": "2025-01-01"
                },
                "model-b": {
                    "name": "Model B",
                    "tool_call": true
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 2);
        // reasoning model should come first
        assert_eq!(models[0].id, "model-a");
        assert!(models[0].reasoning.unwrap_or(false));
    }

    #[test]
    fn test_extract_models_offline_format() {
        // Backward-compat: extract_models handles sparse model data gracefully
        let provider_data = serde_json::json!({
            "name": "Test Offline",
            "models": {
                "model-x": {
                    "id": "model-x",
                    "name": "Model X",
                    "tool_call": true,
                    "limit": { "context": 128000, "output": 4096 }
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-x");
        assert_eq!(models[0].context_window, Some(128000));
        assert_eq!(models[0].max_tokens, Some(4096));
        assert!(models[0].tool_call.unwrap_or(false));
        // Fields not present should be None
        assert!(models[0].family.is_none());
        assert!(models[0].reasoning.is_none());
        assert!(models[0].attachment.is_none());
        assert!(models[0].release_date.is_none());
    }

    #[test]
    fn test_extract_models_with_new_fields() {
        // Verify extract_models reads family, reasoning, attachment from offline data
        let provider_data = serde_json::json!({
            "name": "Test Provider",
            "models": {
                "model-y": {
                    "id": "model-y",
                    "name": "Model Y",
                    "family": "test-family",
                    "reasoning": true,
                    "tool_call": true,
                    "attachment": false,
                    "limit": { "context": 64000, "output": 8192 }
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-y");
        assert_eq!(models[0].family, Some("test-family".to_string()));
        assert_eq!(models[0].reasoning, Some(true));
        assert_eq!(models[0].attachment, Some(false));
        assert_eq!(models[0].context_window, Some(64000));
        assert_eq!(models[0].max_tokens, Some(8192));
    }

    #[test]
    fn test_resolve_provider_from_offline() {
        let data = offline_providers();
        let result = resolve_provider(data, "openai");
        assert!(result.is_some());
        let (name, models) = result.unwrap();
        assert!(!name.is_empty());
        assert!(!models.is_empty());
    }

    #[test]
    fn test_resolve_provider_cn_variant() {
        let data = offline_providers();
        // minimax should resolve both minimax and minimax-cn
        let result = resolve_provider(data, "minimax");
        assert!(result.is_some());
        let (_, models) = result.unwrap();
        assert!(!models.is_empty(), "minimax should have models from both variants");
    }

    #[test]
    fn test_resolve_provider_not_found() {
        let data = offline_providers();
        let result = resolve_provider(data, "nonexistent-provider");
        assert!(result.is_none());
    }
}
