//! Models API — proxy to models.dev for provider model lists
//!
//! Endpoints:
//!   GET /api/models              — list all providers with models
//!   GET /api/models/{provider}   — get models for a specific provider
//!
//! Responses are cached in memory with a TTL of 5 minutes.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::http::routes::AppState;

/// Cache TTL: 5 minutes
const CACHE_TTL_SECS: u64 = 300;

/// models.dev API base URL
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

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

// ── Helpers ───────────────────────────────────────────────────────────

/// Fetch models.dev data, using cache if fresh
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

/// Map our provider IDs to models.dev provider IDs
/// Our IDs are simple ("openai", "minimax"), models.dev may differ.
/// Mapping verified against https://models.dev/api.json on 2026-04-28.
fn to_models_dev_id(provider_id: &str) -> Vec<String> {
    match provider_id {
        "openai" => vec!["openai".to_string()],
        "anthropic" => vec!["anthropic".to_string()],
        "google" | "gemini" => vec!["google".to_string()],
        "groq" => vec!["groq".to_string()],
        "mistral" => vec!["mistral".to_string()],
        "xai" => vec!["xai".to_string()],
        "openrouter" => vec!["openrouter".to_string()],
        "azure" => vec!["azure".to_string()],
        "deepseek" => vec!["deepseek".to_string()],
        // GLM / 智谱: models.dev uses "zhipuai" (not "zhipu")
        "glm" | "zhipu" | "zhipuai" => vec!["zhipuai".to_string(), "zhipuai-coding-plan".to_string()],
        // Moonshot / Kimi: models.dev uses "moonshotai" (not "moonshot")
        "moonshot" | "kimi" => vec!["moonshotai".to_string(), "moonshotai-cn".to_string()],
        // Qwen / 阿里云: models.dev uses "alibaba" (not "qwen" or "dashscope")
        "qwen" | "dashscope" | "alibaba" => vec!["alibaba".to_string(), "alibaba-cn".to_string()],
        "minimax" => vec!["minimax".to_string(), "minimax-cn".to_string()],
        // Doubao / 豆包: not available on models.dev, return empty so
        // the client falls back to hardcoded exampleModels
        "doubao" | "volcengine" => vec![],
        // Ollama local: models.dev has no "ollama" key (only "ollama-cloud"
        // which is a different product). Return empty for local providers.
        "ollama" => vec![],
        // LM Studio: local provider, limited models.dev data
        "lmstudio" => vec!["lmstudio".to_string()],
        _ => vec![provider_id.to_string()],
    }
}

/// Extract models from a models.dev provider JSON object
fn extract_models(provider_data: &serde_json::Value) -> Vec<ModelInfo> {
    let models_obj = match provider_data.get("models") {
        Some(serde_json::Value::Object(m)) => m,
        _ => return Vec::new(),
    };

    let mut models = Vec::new();
    for (id, model_data) in models_obj {
        // Extract limit info if present
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

// ── Handlers ──────────────────────────────────────────────────────────

/// GET /api/models — list all providers with model counts
async fn list_all_providers(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let data = fetch_models(&state.models_cache)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e})),
            )
        })?;

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Invalid models.dev response"})),
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

        result.push(serde_json::json!({
            "id": id,
            "name": name,
            "model_count": model_count,
        }));
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
async fn get_provider_models(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let data = fetch_models(&state.models_cache)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e})),
            )
        })?;

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Invalid models.dev response"})),
            ))
        }
    };

    // Try each possible models.dev ID mapping
    let dev_ids = to_models_dev_id(&provider_id);
    let mut all_models = Vec::new();
    let mut provider_name = provider_id.clone();

    for dev_id in &dev_ids {
        if let Some(provider_data) = providers.get(dev_id) {
            if provider_name == provider_id {
                provider_name = provider_data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&provider_id)
                    .to_string();
            }
            let models = extract_models(provider_data);
            all_models.extend(models);
        }
    }

    // Deduplicate by model id
    let mut seen = std::collections::HashSet::new();
    all_models.retain(|m| seen.insert(m.id.clone()));

    Ok(Json(ProviderModels {
        id: provider_id,
        name: provider_name,
        models: all_models,
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_models_dev_id_minimax() {
        assert_eq!(
            to_models_dev_id("minimax"),
            vec!["minimax".to_string(), "minimax-cn".to_string()]
        );
    }

    #[test]
    fn test_to_models_dev_id_qwen() {
        // models.dev uses "alibaba" for Qwen (not "qwen" or "dashscope")
        assert_eq!(
            to_models_dev_id("qwen"),
            vec!["alibaba".to_string(), "alibaba-cn".to_string()]
        );
        assert_eq!(
            to_models_dev_id("dashscope"),
            vec!["alibaba".to_string(), "alibaba-cn".to_string()]
        );
    }

    #[test]
    fn test_to_models_dev_id_openai() {
        assert_eq!(to_models_dev_id("openai"), vec!["openai".to_string()]);
    }

    #[test]
    fn test_to_models_dev_id_zhipu() {
        // models.dev uses "zhipuai" (not "zhipu")
        assert_eq!(
            to_models_dev_id("glm"),
            vec!["zhipuai".to_string(), "zhipuai-coding-plan".to_string()]
        );
        assert_eq!(
            to_models_dev_id("zhipu"),
            vec!["zhipuai".to_string(), "zhipuai-coding-plan".to_string()]
        );
    }

    #[test]
    fn test_to_models_dev_id_moonshot() {
        // models.dev uses "moonshotai" (not "moonshot")
        assert_eq!(
            to_models_dev_id("moonshot"),
            vec!["moonshotai".to_string(), "moonshotai-cn".to_string()]
        );
        assert_eq!(
            to_models_dev_id("kimi"),
            vec!["moonshotai".to_string(), "moonshotai-cn".to_string()]
        );
    }

    #[test]
    fn test_to_models_dev_id_not_on_models_dev() {
        // doubao and ollama are not available on models.dev
        assert_eq!(to_models_dev_id("doubao"), Vec::<String>::new());
        assert_eq!(to_models_dev_id("volcengine"), Vec::<String>::new());
        assert_eq!(to_models_dev_id("ollama"), Vec::<String>::new());
    }

    #[test]
    fn test_to_models_dev_id_unknown_fallback() {
        // Unknown provider IDs are passed through as-is
        assert_eq!(to_models_dev_id("some-new-provider"), vec!["some-new-provider".to_string()]);
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
}
