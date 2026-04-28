//! Gateway HTTP client
//!
//! Encapsulates all Gateway HTTP API calls. The Desktop App does NOT
//! depend on any rollball internal crate — it communicates with the
//! platform exclusively through Gateway HTTP API and Debug Protocol
//! WebSocket.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Default Gateway base URL
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:19876";

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
        let health: HealthResponse = resp.json().await?;
        Ok(health)
    }

    // ── System Status ──────────────────────────────────────────────────

    /// `GET /api/status`
    pub async fn system_status(&self) -> Result<SystemStatusResponse> {
        let resp = self.client.get(format!("{}/api/status", self.base_url)).send().await?;
        let status: SystemStatusResponse = resp.json().await?;
        Ok(status)
    }

    // ── Agent Management ───────────────────────────────────────────────

    /// `GET /api/agents`
    pub async fn list_agents(&self) -> Result<Vec<AgentListEntry>> {
        let resp = self.client.get(format!("{}/api/agents", self.base_url)).send().await?;
        let agents: Vec<AgentListEntry> = resp.json().await?;
        Ok(agents)
    }

    /// `GET /api/agents/:id`
    pub async fn get_agent_detail(&self, agent_id: &str) -> Result<AgentDetailResponse> {
        let resp = self
            .client
            .get(format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await?;
        let detail: AgentDetailResponse = resp.json().await?;
        Ok(detail)
    }

    /// `POST /api/agents/install`
    pub async fn install_agent(&self, package_path: &str, dev_mode: bool) -> Result<GenericMessageResponse> {
        let body = serde_json::json!({ "package_path": package_path, "dev_mode": dev_mode });
        let resp = self
            .client
            .post(format!("{}/api/agents/install", self.base_url))
            .json(&body)
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    /// `DELETE /api/agents/:id`
    pub async fn uninstall_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    /// `POST /api/agents/:id/start`
    pub async fn start_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/start", self.base_url, agent_id))
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    /// `POST /api/agents/:id/stop`
    pub async fn stop_agent(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/stop", self.base_url, agent_id))
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
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
        // Check for error status
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_else(|_| "unknown error".to_string());
            anyhow::bail!("HTTP {}: {}", status, text);
        }
        let result: SendMessageResponse = resp.json().await?;
        Ok(result)
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
        let keys: Vec<VaultKeyEntry> = resp.json().await?;
        Ok(keys)
    }

    /// `POST /api/vault/keys` (with optional base_url and default_model)
    pub async fn add_key(
        &self,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
        default_model: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::json!({ "provider": provider, "key": key });
        if let Some(url) = base_url {
            body["base_url"] = serde_json::Value::String(url.to_string());
        }
        if let Some(model) = default_model {
            body["default_model"] = serde_json::Value::String(model.to_string());
        }
        let resp = self
            .client
            .post(format!("{}/api/vault/keys", self.base_url))
            .json(&body)
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    /// `DELETE /api/vault/keys/:provider`
    pub async fn remove_key(&self, provider: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/vault/keys/{}", self.base_url, provider))
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    /// `PUT /api/vault/keys/:provider` (with optional base_url and default_model)
    pub async fn update_key(
        &self,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
        default_model: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::json!({ "key": key });
        if let Some(url) = base_url {
            body["base_url"] = serde_json::Value::String(url.to_string());
        }
        if let Some(model) = default_model {
            body["default_model"] = serde_json::Value::String(model.to_string());
        }
        let resp = self
            .client
            .put(format!("{}/api/vault/keys/{}", self.base_url, provider))
            .json(&body)
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
    }

    // ── Config ─────────────────────────────────────────────────────────

    /// `GET /api/config`
    pub async fn get_config(&self) -> Result<ConfigResponse> {
        let resp = self
            .client
            .get(format!("{}/api/config", self.base_url))
            .send()
            .await?;
        let config: ConfigResponse = resp.json().await?;
        Ok(config)
    }

    /// `PUT /api/config`
    pub async fn update_config(
        &self,
        log_level: Option<&str>,
        idle_timeout_secs: Option<u64>,
        default_provider: Option<&str>,
        default_model: Option<&str>,
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
        let resp = self
            .client
            .put(format!("{}/api/config", self.base_url))
            .json(&body)
            .send()
            .await?;
        let result: GenericMessageResponse = resp.json().await?;
        Ok(result)
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

/// Vault key entry (masked, with optional base_url and default_model)
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
