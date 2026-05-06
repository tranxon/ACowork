//! Gateway HTTP client
//!
//! Encapsulates all Gateway HTTP API calls. The Desktop App communicates
//! with the platform primarily through Gateway HTTP API and Debug Protocol
//! WebSocket. It references rollball_core::defaults for shared constants
//! (host, port, URL) to avoid hardcoded duplication.

use anyhow::Result;
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use rollball_core::defaults;

/// Default Gateway base URL (from shared core constants)
const DEFAULT_BASE_URL: &str = defaults::GATEWAY_HTTP_URL;

/// Gateway error response format (matches Gateway's `ApiError` struct)
#[derive(Deserialize)]
struct GatewayErrorResponse {
    error: String,
    #[allow(dead_code)]
    code: u16,
}

/// Unified response parser for all Gateway API calls.
///
/// - Success (2xx): deserializes the response body into `T`.
/// - Failure: attempts to extract the `error` field from Gateway's
///   `ApiError` JSON format for a clear message; falls back to raw text.
async fn parse_gateway_response<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        resp.json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Gateway response: {}", e))
    } else {
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<GatewayErrorResponse>(&text) {
            Ok(err) => anyhow::bail!("Gateway {}: {}", status, err.error),
            Err(_) => anyhow::bail!("Gateway {}: {}", status, text),
        }
    }
}

/// Gateway HTTP client
pub struct GatewayClient {
    client: reqwest::Client,
    base_url: String,
}

impl GatewayClient {
    /// Create a new GatewayClient with the default base URL
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL.to_string())
    }

    /// Create a new GatewayClient with a custom base URL
    pub fn with_base_url(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build reqwest client");
        Self { client, base_url }
    }

    /// Get the current base URL
    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Update the base URL (e.g., from settings)
    #[allow(dead_code)]
    pub fn set_base_url(&mut self, url: String) {
        self.base_url = url;
    }

    // ── Health ──────────────────────────────────────────────────────────

    /// `GET /health`
    pub async fn health(&self) -> Result<HealthResponse> {
        let resp = self.client.get(format!("{}/health", self.base_url)).send().await?;
        parse_gateway_response(resp).await
    }

    // ── System Status ──────────────────────────────────────────────────

    /// `GET /api/status`
    pub async fn system_status(&self) -> Result<SystemStatusResponse> {
        let resp = self.client.get(format!("{}/api/status", self.base_url)).send().await?;
        parse_gateway_response(resp).await
    }

    // ── Agent Management ───────────────────────────────────────────────

    /// `GET /api/agents`
    pub async fn list_agents(&self) -> Result<Vec<AgentListEntry>> {
        let resp = self.client.get(format!("{}/api/agents", self.base_url)).send().await?;
        parse_gateway_response(resp).await
    }

    /// `GET /api/agents/:id`
    pub async fn get_agent_detail(&self, agent_id: &str) -> Result<AgentDetailResponse> {
        let resp = self
            .client
            .get(format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/install` — upload .agent package via multipart
    pub async fn install_agent(&self, package_bytes: &[u8], dev_mode: bool) -> Result<GenericMessageResponse> {
        let form = reqwest::multipart::Form::new()
            .part(
                "package",
                reqwest::multipart::Part::bytes(package_bytes.to_vec())
                    .file_name("package.agent")
                    .mime_str("application/octet-stream")
                    .map_err(|e| anyhow::anyhow!("Invalid mime: {}", e))?,
            )
            .text("dev_mode", dev_mode.to_string());

        let resp = self
            .client
            .post(format!("{}/api/agents/install", self.base_url))
            .multipart(form)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/agents/:id`
    pub async fn uninstall_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/start`
    pub async fn start_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/start", self.base_url, agent_id))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/stop`
    pub async fn stop_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/stop", self.base_url, agent_id))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Chat ───────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/message`
    pub async fn send_message(&self, agent_id: &str, content: &str) -> Result<SendMessageResponse> {
        let body = serde_json::json!({ "content": content });
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/message", self.base_url, agent_id))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// Get the WebSocket URL for streaming chat
    #[allow(dead_code)]
    pub fn stream_url(&self, agent_id: &str) -> String {
        format!("{}/api/agents/{}/stream", self.base_url, agent_id)
            .replace("http://", "ws://")
            .replace("https://", "wss://")
    }

    // ── Vault ──────────────────────────────────────────────────────────

    /// `GET /api/vault/keys`
    pub async fn list_keys(&self) -> Result<Vec<VaultKeyEntry>> {
        let resp = self
            .client
            .get(format!("{}/api/vault/keys", self.base_url))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/vault/keys` (with optional base_url, default_model, models, and model_capabilities)
    pub async fn add_key(
        &self,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
        default_model: Option<&str>,
        models: Option<&[String]>,
        model_capabilities: Option<&ModelCapabilities>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::json!({ "provider": provider, "key": key });
        if let Some(url) = base_url {
            body["base_url"] = serde_json::Value::String(url.to_string());
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models {
            if !models_list.is_empty() {
                body["models"] = serde_json::Value::Array(
                    models_list.iter().map(|m| serde_json::Value::String(m.clone())).collect()
                );
            }
        } else if let Some(model) = default_model {
            body["default_model"] = serde_json::Value::String(model.to_string());
        }
        // Send model_capabilities if provided
        if let Some(caps) = model_capabilities {
            body["model_capabilities"] = serde_json::to_value(caps)
                .unwrap_or_else(|_| serde_json::json!({
                    "context_window": caps.context_window,
                    "max_output_tokens": caps.max_output_tokens,
                    "supports_tool_calling": caps.supports_tool_calling,
                }));
        }
        let resp = self
            .client
            .post(format!("{}/api/vault/keys", self.base_url))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/vault/keys/:provider`
    pub async fn remove_key(&self, provider: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/vault/keys/{}", self.base_url, provider))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `PUT /api/vault/keys/:provider` (supports partial updates — key is optional)
    ///
    /// If `key` is None, the existing API key is preserved on the Gateway side.
    /// This prevents the masked key_preview from overwriting the real key.
    pub async fn update_key(
        &self,
        provider: &str,
        key: Option<&str>,
        base_url: Option<&str>,
        default_model: Option<&str>,
        models: Option<&[String]>,
        model_capabilities: Option<&ModelCapabilities>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::Map::new();
        if let Some(k) = key {
            if !k.is_empty() {
                body.insert("key".to_string(), serde_json::Value::String(k.to_string()));
            }
        }
        if let Some(url) = base_url {
            body.insert("base_url".to_string(), serde_json::Value::String(url.to_string()));
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models {
            if !models_list.is_empty() {
                body.insert(
                    "models".to_string(),
                    serde_json::Value::Array(
                        models_list.iter().map(|m| serde_json::Value::String(m.clone())).collect()
                    ),
                );
            }
        } else if let Some(model) = default_model {
            body.insert("default_model".to_string(), serde_json::Value::String(model.to_string()));
        }
        // Send model_capabilities if provided
        if let Some(caps) = model_capabilities {
            body.insert(
                "model_capabilities".to_string(),
                serde_json::to_value(caps)
                    .unwrap_or_else(|_| serde_json::json!({
                        "context_window": caps.context_window,
                        "max_output_tokens": caps.max_output_tokens,
                        "supports_tool_calling": caps.supports_tool_calling,
                    })),
            );
        }
        let resp = self
            .client
            .put(format!("{}/api/vault/keys/{}", self.base_url, provider))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Config ─────────────────────────────────────────────────────────

    /// `GET /api/config`
    pub async fn get_config(&self) -> Result<ConfigResponse> {
        let resp = self
            .client
            .get(format!("{}/api/config", self.base_url))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `PUT /api/config`
    pub async fn update_config(
        &self,
        log_level: Option<&str>,
        idle_timeout_secs: Option<u64>,
        default_provider: Option<&str>,
        default_model: Option<&str>,
        max_output_tokens_limit: Option<u64>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::Map::new();
        if let Some(level) = log_level {
            body.insert("log_level".to_string(), serde_json::Value::String(level.to_string()));
        }
        if let Some(timeout) = idle_timeout_secs {
            body.insert(
                "idle_timeout_secs".to_string(),
                serde_json::Value::Number(timeout.into()),
            );
        }
        if let Some(provider) = default_provider {
            body.insert("default_provider".to_string(), serde_json::Value::String(provider.to_string()));
        }
        if let Some(model) = default_model {
            body.insert("default_model".to_string(), serde_json::Value::String(model.to_string()));
        }
        if let Some(limit) = max_output_tokens_limit {
            body.insert(
                "max_output_tokens_limit".to_string(),
                serde_json::Value::Number(limit.into()),
            );
        }
        let resp = self
            .client
            .put(format!("{}/api/config", self.base_url))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }
}

// ── API response types ──────────────────────────────────────────────

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    #[serde(default)]
    pub checks: std::collections::HashMap<String, CheckResult>,
}

/// Individual check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// System status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatusResponse {
    pub version: String,
    pub agents_installed: usize,
    pub agents_running: usize,
    pub uptime_secs: u64,
}

/// Agent list entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListEntry {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub running: bool,
}

/// Agent detail response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetailResponse {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
}

/// Generic message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericMessageResponse {
    pub message: String,
}

/// Send message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub message_id: String,
    pub status: String,
}

/// Vault key entry (masked, with optional base_url, default_model, models list, and model capabilities)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultKeyEntry {
    pub provider: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Configured default model (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selected models list (may be empty)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Model capabilities (from models.dev or user input)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_capabilities: Option<ModelCapabilities>,
}

/// Model capabilities info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Context window size (total tokens: input + output)
    pub context_window: u64,
    /// Maximum output tokens the model can generate
    pub max_output_tokens: u64,
    /// Maximum input tokens (from models.dev limit.input)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Whether the model supports tool/function calling
    #[serde(default = "default_true")]
    pub supports_tool_calling: bool,
    /// Whether the model supports reasoning/thinking
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Whether the model supports file attachments
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_attachment: Option<bool>,
    /// Whether the model supports temperature parameter
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    /// Pricing information (USD per 1M tokens)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostInfo>,
    /// Supported modalities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelModalities>,
    /// Model display name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Model family
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Knowledge cutoff date
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,
}

/// Cost information for a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostInfo {
    /// Input cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_million: Option<f64>,
    /// Output cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_million: Option<f64>,
}

/// Modality information for a model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelModalities {
    /// Input modalities
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Output modalities
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Config response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub socket_path: String,
    pub packages_dir: String,
    pub data_dir: String,
    pub log_level: String,
    pub idle_timeout_secs: u64,
    pub dev_mode: bool,
    pub http: HttpConfigResponse,
    /// Default LLM provider (if configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Default LLM model (if configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// HTTP config subset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfigResponse {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub auth_enabled: bool,
}
