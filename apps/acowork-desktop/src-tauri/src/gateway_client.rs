//! Gateway HTTP client
//!
//! Encapsulates all Gateway HTTP API calls. The Desktop App communicates
//! with the platform primarily through Gateway HTTP API and Debug Protocol
//! WebSocket. It references acowork_core::defaults for shared constants
//! (host, port, URL) to avoid hardcoded duplication.

use acowork_core::defaults;
use anyhow::Result;
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Default Gateway base URL (from shared core constants)
const DEFAULT_BASE_URL: &str = defaults::GATEWAY_HTTP_URL;

/// Minimal RFC 3986 percent-encoder for query string values. Used to embed
/// user-supplied file paths in the upload endpoint URL without pulling in
/// the `urlencoding` crate just for this single use case.
fn urlencoded(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

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

    // ── Agent Management ───────────────────────────────────────────────

    /// `GET /api/agents`
    pub async fn list_agents(&self) -> Result<Vec<AgentListEntry>> {
        let resp = self
            .client
            .get(format!("{}/api/agents", self.base_url))
            .send()
            .await?;
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
    pub async fn install_agent(
        &self,
        package_bytes: &[u8],
        dev_mode: bool,
    ) -> Result<GenericMessageResponse> {
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

    /// `POST /api/agents/:id/manifest/avatar` — write avatar / builtin_avatar
    /// fields into the agent's installed manifest.toml.
    ///
    /// Pass `Some("...")` to set, `Some("")` or `None` to clear (omit to leave
    /// unchanged). Used by the Publish wizard to bake the user's avatar
    /// selection into the package before build.
    pub async fn update_agent_manifest_avatar(
        &self,
        agent_id: &str,
        avatar: Option<&str>,
        builtin_avatar: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::Map::new();
        if let Some(v) = avatar {
            body.insert("avatar".to_string(), serde_json::Value::String(v.to_string()));
        }
        if let Some(v) = builtin_avatar {
            body.insert(
                "builtin_avatar".to_string(),
                serde_json::Value::String(v.to_string()),
            );
        }
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/manifest/avatar",
                self.base_url, agent_id
            ))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/manifest/file?path=<relative>` — write a single
    /// file into the agent's install directory. Used by the Publish wizard to
    /// upload a custom avatar image. The path is restricted to image
    /// extensions server-side.
    pub async fn upload_agent_file(
        &self,
        agent_id: &str,
        relative_path: &str,
        bytes: &[u8],
    ) -> Result<serde_json::Value> {
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name("upload")
                .mime_str("application/octet-stream")
                .map_err(|e| anyhow::anyhow!("Invalid mime: {}", e))?,
        );
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/manifest/file?path={}",
                self.base_url,
                agent_id,
                urlencoded(relative_path)
            ))
            .multipart(form)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/user/avatar-file` — upload a user avatar image.
    /// Returns `{ "relative_path": "assets/avatar-01.png" }`.
    pub async fn upload_user_avatar_file(
        &self,
        bytes: &[u8],
        file_name: &str,
    ) -> Result<serde_json::Value> {
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(file_name.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e| anyhow::anyhow!("Invalid mime: {}", e))?,
        );
        let resp = self
            .client
            .post(format!("{}/api/user/avatar-file", self.base_url))
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
    pub async fn start_agent(
        &self,
        agent_id: &str,
        dev_mode: bool,
    ) -> Result<GenericMessageResponse> {
        let body = serde_json::json!({ "dev_mode": dev_mode });
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/start", self.base_url, agent_id))
            .json(&body)
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

    /// `POST /api/agents/:id/restart-debug`
    pub async fn restart_agent_in_debug(&self, agent_id: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/restart-debug",
                self.base_url, agent_id
            ))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Clone ──────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/clone`
    pub async fn clone_agent(
        &self,
        agent_id: &str,
        new_agent_id: &str,
        mode: &str,
    ) -> Result<CloneResponse> {
        let body = serde_json::json!({
            "new_agent_id": new_agent_id,
            "mode": mode,
        });
        let resp = self
            .client
            .post(format!("{}/api/agents/{}/clone", self.base_url, agent_id))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Publish ────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/publish/prepare`
    pub async fn prepare_publish(
        &self,
        agent_id: &str,
        clean: bool,
    ) -> Result<PreparePublishResponse> {
        let body = serde_json::json!({ "clean": clean });
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/publish/prepare",
                self.base_url, agent_id
            ))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/publish/build`
    pub async fn build_publish(
        &self,
        agent_id: &str,
        sign: bool,
        key_dir: Option<&str>,
    ) -> Result<BuildPublishResponse> {
        let mut body = serde_json::json!({ "sign": sign });
        if let Some(dir) = key_dir {
            body["key_dir"] = serde_json::Value::String(dir.to_string());
        }
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/publish/build",
                self.base_url, agent_id
            ))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/agents/:id/publish/export`
    pub async fn export_package(&self, agent_id: &str) -> Result<ExportPackageResponse> {
        let resp = self
            .client
            .post(format!(
                "{}/api/agents/{}/publish/export",
                self.base_url, agent_id
            ))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Chat ───────────────────────────────────────────────────────────

    /// `POST /api/agents/:id/message`
    pub async fn send_message(
        &self,
        agent_id: &str,
        content: &str,
        session_id: Option<&str>,
        command: Option<&str>,
        document_ids: Option<&[String]>,
        attached_context: Option<&[serde_json::Value]>,
    ) -> Result<SendMessageResponse> {
        let mut body = serde_json::json!({ "content": content });
        if let Some(sid) = session_id {
            body["session_id"] = serde_json::json!(sid);
        }
        if let Some(cmd) = command {
            body["command"] = serde_json::json!(cmd);
        }
        if let Some(ids) = document_ids {
            if !ids.is_empty() {
                body["document_ids"] = serde_json::json!(ids);
            }
        }
        if let Some(ctx) = attached_context {
            if !ctx.is_empty() {
                body["attached_context"] = serde_json::json!(ctx);
            }
        }
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

    // ── Documents ──────────────────────────────────────────────────────

    /// `POST /api/sessions/:session_id/documents` — multipart upload
    pub async fn upload_document(
        &self,
        session_id: &str,
        file_path: &str,
    ) -> Result<DocumentUploadResponse> {
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document.bin");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("filename", file_name.to_string());

        let resp = self
            .client
            .post(format!(
                "{}/api/sessions/{}/documents",
                self.base_url, session_id
            ))
            .multipart(form)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Vault ──────────────────────────────────────────────────────────

    /// `GET /api/providers`
    pub async fn list_keys(&self) -> Result<Vec<VaultKeyEntry>> {
        let resp = self
            .client
            .get(format!("{}/api/providers", self.base_url))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/providers` (with optional base_url, default_model, models, model_capabilities, and custom flag)
    pub async fn add_key(
        &self,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
        default_model: Option<&str>,
        models: Option<&[String]>,
        model_capabilities: &HashMap<String, ModelCapabilities>,
        compact_model: Option<&str>,
        custom: bool,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::json!({ "provider": provider, "key": key });
        if custom {
            body["custom"] = serde_json::Value::Bool(true);
        }
        if let Some(url) = base_url {
            body["base_url"] = serde_json::Value::String(url.to_string());
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models {
            if !models_list.is_empty() {
                body["models"] = serde_json::Value::Array(
                    models_list
                        .iter()
                        .map(|m| serde_json::Value::String(m.clone()))
                        .collect(),
                );
            }
        } else if let Some(model) = default_model {
            body["default_model"] = serde_json::Value::String(model.to_string());
        }
        // Send model_capabilities if not empty
        if !model_capabilities.is_empty() {
            body["model_capabilities"] =
                serde_json::to_value(model_capabilities).unwrap_or_else(|e| {
                    eprintln!("serde_json::to_value failed for model_capabilities: {e}");
                    serde_json::to_value(model_capabilities)
                        .expect("model_capabilities serialization failed twice")
                });
        }
        // Send compact_model if provided
        if let Some(cm) = compact_model {
            body["compact_model"] = serde_json::Value::String(cm.to_string());
        }
        let resp = self
            .client
            .post(format!("{}/api/providers", self.base_url))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/providers/:provider`
    pub async fn remove_key(&self, provider: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/providers/{}", self.base_url, provider))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `PUT /api/providers/:provider` (supports partial updates — key is optional)
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
        model_capabilities: &HashMap<String, ModelCapabilities>,
        compact_model: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::Map::new();
        if let Some(k) = key {
            if !k.is_empty() {
                body.insert("key".to_string(), serde_json::Value::String(k.to_string()));
            }
        }
        if let Some(url) = base_url {
            body.insert(
                "base_url".to_string(),
                serde_json::Value::String(url.to_string()),
            );
        }
        // Send models list if provided; otherwise fallback to default_model
        if let Some(models_list) = models {
            if !models_list.is_empty() {
                body.insert(
                    "models".to_string(),
                    serde_json::Value::Array(
                        models_list
                            .iter()
                            .map(|m| serde_json::Value::String(m.clone()))
                            .collect(),
                    ),
                );
            }
        } else if let Some(model) = default_model {
            body.insert(
                "default_model".to_string(),
                serde_json::Value::String(model.to_string()),
            );
        }
        // Send model_capabilities if not empty
        if !model_capabilities.is_empty() {
            body.insert(
                "model_capabilities".to_string(),
                serde_json::to_value(model_capabilities).unwrap_or_else(|e| {
                    eprintln!("serde_json::to_value failed for model_capabilities: {e}");
                    serde_json::to_value(model_capabilities)
                        .expect("model_capabilities serialization failed twice")
                }),
            );
        }
        // Send compact_model if provided
        if let Some(cm) = compact_model {
            body.insert(
                "compact_model".to_string(),
                serde_json::Value::String(cm.to_string()),
            );
        }
        let resp = self
            .client
            .put(format!("{}/api/providers/{}", self.base_url, provider))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Search Keys ─────────────────────────────────────────────────────

    /// `GET /api/search/keys` — list search provider keys (masked)
    pub async fn list_search_keys(&self) -> Result<Vec<SearchVaultKeyEntry>> {
        let resp = self
            .client
            .get(format!("{}/api/search/keys", self.base_url))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `POST /api/search/keys` — add a search provider key
    pub async fn add_search_key(
        &self,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::json!({ "provider": provider, "key": key });
        if let Some(url) = base_url {
            if !url.is_empty() {
                body["base_url"] = serde_json::Value::String(url.to_string());
            }
        }
        let resp = self
            .client
            .post(format!("{}/api/search/keys", self.base_url))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `DELETE /api/search/keys/:provider` — remove a search provider key
    pub async fn remove_search_key(&self, provider: &str) -> Result<GenericMessageResponse> {
        let resp = self
            .client
            .delete(format!("{}/api/search/keys/{}", self.base_url, provider))
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    /// `PUT /api/search/keys/:provider` — update a search provider key (partial)
    pub async fn update_search_key(
        &self,
        provider: &str,
        key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<GenericMessageResponse> {
        let mut body = serde_json::Map::new();
        if let Some(k) = key {
            if !k.is_empty() {
                body.insert("key".to_string(), serde_json::Value::String(k.to_string()));
            }
        }
        if let Some(url) = base_url {
            body.insert(
                "base_url".to_string(),
                serde_json::Value::String(url.to_string()),
            );
        }
        let resp = self
            .client
            .put(format!("{}/api/search/keys/{}", self.base_url, provider))
            .json(&body)
            .send()
            .await?;
        parse_gateway_response(resp).await
    }

    // ── Config ─────────────────────────────────────────────────────────
    //
    // Config and log management are now handled by the frontend directly
    // via fetch() to the Gateway HTTP API (getGatewayUrl()).
}

// ── API response types ──────────────────────────────────────────────

/// Agent list entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListEntry {
    pub agent_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
    pub version: String,
    pub running: bool,
    pub connected: bool,
    pub ready: bool,
    pub dev_mode: bool,
    pub debug_port: Option<u16>,
}

/// Agent detail response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetailResponse {
    pub agent_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    pub running: bool,
    pub connected: bool,
    pub ready: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub debug_port: Option<u16>,
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

/// Clone response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneResponse {
    pub agent_id: String,
    pub install_path: String,
}

/// A single check item from publish prepare
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckItem {
    pub field: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Publish prepare response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparePublishResponse {
    pub checks: Vec<CheckItem>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub cleaned: bool,
}

/// Publish build response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPublishResponse {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// Export package response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPackageResponse {
    pub status: String,
    pub output_path: String,
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
    /// Per-model capabilities
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_capabilities: HashMap<String, ModelCapabilities>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a local (self-hosted) provider (no API key required)
    #[serde(default)]
    pub local: bool,
    /// Whether this is a user-defined custom provider (not listed in models.dev)
    #[serde(default)]
    pub custom: bool,
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
    /// Default reasoning effort level (user-configured). Values: "off", "low", "medium", "high", "max".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    /// Anthropic thinking mode: "extended" or "adaptive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<String>,
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

// ── Search key types ─────────────────────────────────────────────────

/// Search vault key entry (masked)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchVaultKeyEntry {
    pub provider: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

// ── Document upload ───────────────────────────────────────────────────

/// Response from `POST /api/sessions/{sid}/documents`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadResponse {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
}
