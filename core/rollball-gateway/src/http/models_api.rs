//! Models API — provider model lists with offline fallback
//!
//! Endpoints:
//!   GET /api/models              — list all providers with models
//!   GET /api/models/{provider}   — get models for a specific provider
//!
//! Data sources (in priority order):
//!   1. In-memory cache (TTL 5 minutes)
//!   2. models.dev API (https://models.dev/api.json)
//!   3. Built-in offline data (offline_providers.json via include_str!)
//!   4. Empty result

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
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

/// Built-in offline provider data (compiled into binary)
const OFFLINE_PROVIDERS: &str = include_str!("offline_providers.json");

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
    pub release_date: Option<String>,
    /// Context window size (total tokens: input + output)
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Maximum output tokens
    #[serde(default)]
    pub max_tokens: Option<u64>,
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

// ── Offline data ──────────────────────────────────────────────────────

/// Load built-in offline provider data (parsed once, cached forever)
fn offline_providers() -> &'static serde_json::Value {
    static DATA: OnceLock<serde_json::Value> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(OFFLINE_PROVIDERS).expect("Invalid offline_providers.json")
    })
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

/// Fetch models.dev data, using cache if fresh.
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
            release_date: model_data
                .get("release_date")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            context_window,
            max_tokens,
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
async fn list_all_providers(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Try models.dev cache/API first; fall back to offline data on failure
    let data = match fetch_models(&state.models_cache).await {
        Ok(d) => d,
        Err(_) => offline_providers().clone(),
    };

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
///   1. models.dev cache/API (direct provider_id lookup, no alias mapping)
///   2. Built-in offline data
///   3. Empty result
async fn get_provider_models(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // 1. Try models.dev cache/API
    if let Ok(data) = fetch_models(&state.models_cache).await {
        if let Some((name, models)) = resolve_provider(&data, &provider_id) {
            return Ok(Json(ProviderModels {
                id: provider_id,
                name,
                models,
            }));
        }
    }

    // 2. Try offline data
    if let Some((name, models)) = resolve_provider(offline_providers(), &provider_id) {
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

// ── Tests ─────────────────────────────────────────────────────────────

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
            "zhipuai", "zhipuai-coding-plan", "moonshotai", "moonshotai-cn",
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
            "deepseek", "minimax", "minimax-cn", "zhipuai", "zhipuai-coding-plan",
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
