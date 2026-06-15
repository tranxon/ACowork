//! Models API — provider model lists (offline-only)
//!
//! Endpoints:
//!   GET /api/models              — list all providers with models
//!   GET /api/models/{provider}   — get models for a specific provider
//!
//! Data source: offline_providers.json loaded at startup into a static cache.

use axum::{Json, Router, extract::Path, http::StatusCode, response::IntoResponse, routing::get};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::http::routes::AppState;

// ── Local provider registry ───────────────────────────────────────────

/// Static registry of local (self-hosted) LLM providers.
///
/// Local providers do NOT require an API key and are NOT listed on
/// models.dev. Their models must be discovered at runtime by querying
/// the local server (e.g. ollama `/api/tags`, LMStudio `/v1/models`).
///
/// To add a new local provider, add a tuple here — no other changes needed.
///   (id, display_name, default_base_url)
const LOCAL_PROVIDERS: &[(&str, &str, &str)] = &[
    ("ollama", "Ollama (Local)", "http://localhost:11434"),
    ("lmstudio", "LM Studio (Local)", "http://localhost:1234/v1"),
];

/// Check whether a provider ID refers to a local (self-hosted) provider.
///
/// Used by vault_api.rs to skip API key validation for local providers,
/// and by the frontend to determine UI treatment (no key input, local badge).
pub fn is_local_provider(id: &str) -> bool {
    LOCAL_PROVIDERS.iter().any(|(pid, _, _)| *pid == id)
}

/// Get the default base URL for a local provider, if it exists in the registry.
pub fn local_provider_default_url(id: &str) -> Option<&'static str> {
    LOCAL_PROVIDERS
        .iter()
        .find_map(|(pid, _, url)| if *pid == id { Some(*url) } else { None })
}

/// Derive the protocol type for a local provider.
///
/// ollama → Ollama (native API), lmstudio → OpenAI (OpenAI-compatible).
/// Other local providers default to OpenAI-compatible.
fn local_protocol_type(id: &str) -> acowork_core::protocol::ProtocolType {
    use acowork_core::protocol::ProtocolType;
    match id {
        "ollama" => ProtocolType::Ollama,
        _ => ProtocolType::OpenAI,
    }
}

// ── CN variant helpers ────────────────────────────────────────────────

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

// ── Route builder ─────────────────────────────────────────────────────

pub fn models_routes() -> Router<AppState> {
    Router::new()
        .route("/api/models", get(list_all_providers))
        .route("/api/models/{provider}", get(get_provider_models))
}

// ── Offline data ──────────────────────────────────────────────────────

/// Load offline provider data from a file on disk.
///
/// Search order:
///   1. $CARGO_MANIFEST_DIR/../../assets/offline_providers.json  (dev / test via cargo)
///   2. {exe_dir}/offline_providers.json                          (installer-provided)
///   3. {cwd}/offline_providers.json                              (dev convenience)
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
                Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
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
                },
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

    tracing::warn!("offline_providers.json not found in any candidate path, using empty data");
    None
}

fn build_offline_file_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. CARGO_MANIFEST_DIR ../../assets/  (dev and test via cargo)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let assets = std::path::PathBuf::from(&manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("offline_providers.json");
        if assets.exists() {
            candidates.push(assets);
        }
    }

    // 1. Same directory as the executable (installer-provided, read-only)
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("offline_providers.json"));
    }

    // 2. Current working directory (dev convenience)
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
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                }),
            output_modalities: model_data
                .get("modalities")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                }),
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

    Some((
        provider_name.unwrap_or_else(|| provider_id.to_string()),
        all_models,
    ))
}

// ── Handlers ──────────────────────────────────────────────────────────

/// GET /api/models — list all providers with model counts
///
/// Returns offline provider data from the static in-memory cache.
async fn list_all_providers() -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let data = offline_providers().clone();

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Invalid provider data"})),
            ));
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
            "local": false,
        });
        // Include the provider's base API URL when available (from models.dev or offline data)
        if let Some(api_url) = provider_data.get("api").and_then(|v| v.as_str()) {
            entry["api"] = serde_json::json!(api_url);
        }
        result.push(entry);
    }

    // Append / overlay local providers from the static registry.
    // Most local providers don't appear in models.dev, but some (e.g. lmstudio) do.
    for (id, name, default_url) in LOCAL_PROVIDERS {
        if let Some(existing) = result
            .iter_mut()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
        {
            // Already present in models.dev data — override to mark as local
            existing["local"] = serde_json::json!(true);
            existing["name"] = serde_json::json!(name);
            if existing.get("api").is_none() {
                existing["api"] = serde_json::json!(default_url);
            }
        } else {
            result.push(serde_json::json!({
                "id": id,
                "name": name,
                "model_count": 0,
                "local": true,
                "api": default_url,
            }));
        }
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
///   0. (local providers only) Query the running local server directly
///   1. Built-in offline data (instant, always available)
///   2. Empty result
///
/// For local providers (ollama, lmstudio), their models depend on what the
/// user has actually loaded in their local server. We query the server first
/// (with a short timeout), then return empty if unreachable.
///
/// For remote providers, returns offline data only.
async fn get_provider_models(
    Path(provider_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // 0. For local providers, try querying the running server first
    if is_local_provider(&provider_id) {
        if let Some(models) = fetch_local_server_models(&provider_id).await {
            let name = LOCAL_PROVIDERS
                .iter()
                .find(|(pid, _, _)| *pid == provider_id)
                .map(|(_, n, _)| n.to_string())
                .unwrap_or_else(|| provider_id.clone());
            return Ok(Json(ProviderModels {
                id: provider_id,
                name,
                models,
            }));
        }
        // Server unreachable — offline data is meaningless for local
        // providers (user didn't download those models), return empty.
        tracing::debug!(
            "Local provider {} server not reachable, returning empty model list",
            provider_id
        );
        let name = LOCAL_PROVIDERS
            .iter()
            .find(|(pid, _, _)| *pid == provider_id)
            .map(|(_, n, _)| n.to_string())
            .unwrap_or_else(|| provider_id.clone());
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models: vec![],
        }));
    }

    // 1. Try offline data (instant, no network)
    if let Some((name, models)) = resolve_provider(offline_providers(), &provider_id) {
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models,
        }));
    }

    // 2. Provider not found — return empty model list
    Ok(Json(ProviderModels {
        id: provider_id.clone(),
        name: provider_id,
        models: vec![],
    }))
}

/// Try to discover models from a running local provider server.
///
/// Returns `Some(models)` on success (server responded), `None` if the
/// server is not reachable or the response cannot be parsed.
///
/// Supported providers & endpoints:
///   - lmstudio: `GET {base_url}/models` (OpenAI-compatible)
///   - ollama:   `GET {base_url}/api/tags`
async fn fetch_local_server_models(provider_id: &str) -> Option<Vec<ModelInfo>> {
    let base_url = local_provider_default_url(provider_id)?;

    // Shared HTTP client with a short timeout — if the local server isn't
    // running we don't want the caller hanging for more than 2 seconds.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;

    let mut models = match provider_id {
        "lmstudio" => fetch_lmstudio_models(&client, base_url).await,
        "ollama" => fetch_ollama_models(&client, base_url).await,
        _ => None,
    }?;

    // Enrich each model with known capabilities from offline_providers.json.
    // LM Studio / Ollama only returns model IDs — context_window, max_tokens,
    // tool_call support etc. come from the offline data when available.
    for model in &mut models {
        enrich_model_from_offline(model);
    }

    Some(models)
}

/// Parse an OpenAI-compatible `/v1/models` response into `ModelInfo`.
async fn fetch_lmstudio_models(client: &reqwest::Client, base_url: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let data = body.get("data")?.as_array()?;

    Some(
        data.iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?;
                Some(ModelInfo {
                    id: id.to_string(),
                    name: id.to_string(),
                    family: None,
                    reasoning: None,
                    tool_call: None,
                    attachment: None,
                    temperature: None,
                    release_date: None,
                    context_window: None,
                    max_tokens: None,
                    max_input_tokens: None,
                    knowledge: None,
                    input_cost: None,
                    output_cost: None,
                    input_modalities: None,
                    output_modalities: None,
                })
            })
            .collect(),
    )
}

/// Parse an Ollama `/api/tags` response into `ModelInfo`.
async fn fetch_ollama_models(client: &reqwest::Client, base_url: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let models = body.get("models")?.as_array()?;

    Some(
        models
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?;
                // Strip `:latest` suffix for cleaner display
                let id = name.strip_suffix(":latest").unwrap_or(name).to_string();
                Some(ModelInfo {
                    id: id.clone(),
                    name: id,
                    family: None,
                    reasoning: None,
                    tool_call: None,
                    attachment: None,
                    temperature: None,
                    release_date: None,
                    context_window: None,
                    max_tokens: None,
                    max_input_tokens: None,
                    knowledge: None,
                    input_cost: None,
                    output_cost: None,
                    input_modalities: None,
                    output_modalities: None,
                })
            })
            .collect(),
    )
}

/// Look up a model ID in `offline_providers.json` and copy known capabilities
/// (context_window, max_tokens, tool_call, reasoning, etc.) into the model.
///
/// Search strategy:
///   1. Exact match — search all providers for the full model ID
///   2. Bare ID — if the model ID contains a path (e.g. "google/gemma-4-26b-a4b"),
///      try the last segment ("gemma-4-26b-a4b") as well
///
/// NOTE: Suffix-based matching (e.g. adding "-it" or stripping "-instruct") is
/// deliberately NOT done — it's unreliable guessing. The caller in the frontend
/// must always fall back to safe defaults when enrichment finds no match.
///
/// This allows locally-discovered models (which only have an ID) to inherit
/// capabilities from the offline data when available.
fn enrich_model_from_offline(model: &mut ModelInfo) {
    let data = offline_providers();
    let providers = match data.as_object() {
        Some(obj) => obj,
        None => return,
    };

    // Build list of candidate IDs to try (ordered by specificity)
    let mut candidates: Vec<String> = Vec::new();

    // 1. Exact match
    candidates.push(model.id.clone());

    // 2. Bare ID
    if let Some(bare) = model.id.split('/').next_back() {
        if bare != model.id.as_str() {
            candidates.push(bare.to_string());
        }
    }

    for candidate in &candidates {
        for provider_data in providers.values() {
            let models_map = match provider_data.get("models").and_then(|m| m.as_object()) {
                Some(m) => m,
                None => continue,
            };
            let model_data = match models_map.get(candidate) {
                Some(m) => m,
                None => continue,
            };

            // Found a match — copy capabilities
            if let Some(cw) = model_data
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(|v| v.as_u64())
            {
                model.context_window = Some(cw);
            }
            if let Some(mt) = model_data
                .get("limit")
                .and_then(|l| l.get("output"))
                .and_then(|v| v.as_u64())
            {
                model.max_tokens = Some(mt);
            }
            if let Some(tc) = model_data.get("tool_call").and_then(|v| v.as_bool()) {
                model.tool_call = Some(tc);
            }
            if let Some(r) = model_data.get("reasoning").and_then(|v| v.as_bool()) {
                model.reasoning = Some(r);
            }
            if let Some(a) = model_data.get("attachment").and_then(|v| v.as_bool()) {
                model.attachment = Some(a);
            }
            if let Some(name) = model_data.get("name").and_then(|v| v.as_str()) {
                model.name = name.to_string();
            }
            if let Some(family) = model_data.get("family").and_then(|v| v.as_str()) {
                model.family = Some(family.to_string());
            }
            return; // Found the best match, stop searching
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
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    let data = offline_providers();
    lookup_model_capabilities_from_data(data, provider, model_id)
}

/// Internal helper: look up model capabilities from a JSON data source.
fn lookup_model_capabilities_from_data(
    data: &serde_json::Value,
    provider: &str,
    model_id: &str,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
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
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    let bare_id = if model_id.contains('/') {
        model_id.split('/').next_back().unwrap_or(model_id)
    } else {
        model_id
    };

    let providers = data.as_object()?;
    for (pid, provider_data) in providers {
        if let Some(models) = extract_models(provider_data)
            .into_iter()
            .find(|m| m.id == bare_id || m.id == model_id)
        {
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
fn derive_protocol_type(npm: Option<&str>) -> acowork_core::protocol::ProtocolType {
    use acowork_core::protocol::ProtocolType;
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
) -> (acowork_core::protocol::ProtocolType, Option<String>) {
    let data = offline_providers();
    lookup_protocol_info_from_data(data, provider_id, model_id)
}

/// Internal helper: look up protocol info from a JSON data source.
///
/// Priority: local provider registry > model-level npm > provider-level npm > default OpenAI.
/// Model-level api override is returned when present in the model's provider block.
fn lookup_protocol_info_from_data(
    data: &serde_json::Value,
    provider_id: &str,
    model_id: Option<&str>,
) -> (acowork_core::protocol::ProtocolType, Option<String>) {
    use acowork_core::protocol::ProtocolType;

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            // Check local provider registry when no offline data available
            if is_local_provider(provider_id) {
                let proto = local_protocol_type(provider_id);
                let url = local_provider_default_url(provider_id).map(|s| s.to_string());
                return (proto, url);
            }
            return (ProtocolType::OpenAI, None);
        }
    };

    // Find the provider object
    let provider_obj = match providers.get(provider_id) {
        Some(p) => p,
        None => {
            // Check local provider registry when not found in offline data
            if is_local_provider(provider_id) {
                let proto = local_protocol_type(provider_id);
                let url = local_provider_default_url(provider_id).map(|s| s.to_string());
                return (proto, url);
            }
            return (ProtocolType::OpenAI, None);
        }
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
        let model_api = model_provider
            .get("api")
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
    let provider_api = provider_obj
        .get("api")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    (derive_protocol_type(provider_npm), provider_api)
}

/// Convert a ModelInfo to ModelCapabilitiesInfo
fn model_to_capabilities(
    model: &ModelInfo,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    Some(acowork_core::protocol::ModelCapabilitiesInfo {
        context_window: model.context_window.unwrap_or(0),
        max_output_tokens: model.max_tokens.unwrap_or(0),
        max_input_tokens: model.max_input_tokens,
        supports_tool_calling: model.tool_call.unwrap_or(true),
        supports_reasoning: model.reasoning,
        supports_attachment: model.attachment,
        supports_temperature: model.temperature,
        cost: match (model.input_cost, model.output_cost) {
            (Some(inp), Some(out)) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: Some(out),
            }),
            (Some(inp), None) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: None,
            }),
            (None, Some(out)) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: None,
                output_per_million: Some(out),
            }),
            (None, None) => None,
        },
        modalities: match (&model.input_modalities, &model.output_modalities) {
            (Some(inp), Some(out)) => Some(acowork_core::protocol::ModelModalities {
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
            "openai",
            "anthropic",
            "google",
            "deepseek",
            "minimax",
            "minimax-cn",
            "zhipuai",
            "moonshotai",
            "moonshotai-cn",
            "alibaba",
            "alibaba-cn",
            "groq",
            "mistral",
            "xai",
            "openrouter",
            "azure",
            "lmstudio",
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
        assert!(
            openai.get("models").is_some(),
            "provider must have 'models'"
        );

        // Check a model has required fields
        let models = openai["models"].as_object().unwrap();
        let (_, first_model) = models.iter().next().unwrap();
        assert!(first_model.get("id").is_some(), "model must have 'id'");
        assert!(first_model.get("name").is_some(), "model must have 'name'");
        assert!(
            first_model.get("family").is_some(),
            "model must have 'family'"
        );
        assert!(
            first_model.get("reasoning").is_some(),
            "model must have 'reasoning'"
        );
        assert!(
            first_model.get("attachment").is_some(),
            "model must have 'attachment'"
        );
        assert!(
            first_model.get("tool_call").is_some(),
            "model must have 'tool_call'"
        );
        assert!(
            first_model.get("limit").is_some(),
            "model must have 'limit'"
        );
    }

    #[test]
    fn test_offline_provider_has_api_field_when_expected() {
        let data = offline_providers();
        // Providers that should have an api field in the source data
        let providers_with_api = [
            "deepseek",
            "minimax",
            "minimax-cn",
            "zhipuai",
            "moonshotai",
            "moonshotai-cn",
            "alibaba",
            "alibaba-cn",
            "openrouter",
            "lmstudio",
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
        assert_eq!(provider_ids_to_query("openai"), vec!["openai".to_string()]);
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
        assert!(
            !models.is_empty(),
            "minimax should have models from both variants"
        );
    }

    #[test]
    fn test_resolve_provider_not_found() {
        let data = offline_providers();
        let result = resolve_provider(data, "nonexistent-provider");
        assert!(result.is_none());
    }
}
