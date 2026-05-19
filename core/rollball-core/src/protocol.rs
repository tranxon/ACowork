//! Gateway Service API message definitions (contract layer, transport-agnostic)
//!
//! Defines the protocol between Agent Runtime and Gateway.
//! All messages are JSON-serializable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::UsageReport;
use crate::identity::IdentityEntry;

/// Default connection role for backward compatibility
fn default_connection_role() -> String {
    "main".to_string()
}

/// Default value for boolean fields that should default to true
fn default_true() -> bool {
    true
}

/// Default max output tokens limit (32K) — matches opencode's Math.min(limit.output, 32000)
fn default_max_output_tokens_limit() -> u64 {
    32_768
}

/// Cost information for a model (per million tokens)
///
/// Used by BudgetGuard for cost-aware token budgeting.
/// Values are in USD per 1M tokens.
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
///
/// Describes what input/output formats the model supports.
/// Used for future multimodal routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelModalities {
    /// Input modalities (e.g. "text", "image", "audio", "video")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Output modalities (e.g. "text", "image")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

/// Model capabilities info (queried from models.dev / offline data)
///
/// Populated by Gateway when delivering LLM config to Agent Runtime.
/// The Runtime uses this to adapt max_tokens, budget tracking, and
/// other parameters without hardcoding model limits in manifests.
///
/// Design principle: carry as much models.dev data as possible to
/// avoid future protocol changes. All new fields are optional with
/// serde defaults for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilitiesInfo {
    // ── Limit (core, always populated from models.dev) ──
    /// Context window size (total tokens: input + output)
    pub context_window: u64,
    /// Maximum output tokens the model can generate
    pub max_output_tokens: u64,
    /// Maximum input tokens (optional, from models.dev limit.input).
    /// When available, usable context = max_input_tokens - reserved.
    /// When absent, usable context = context_window - max_output_tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,

    // ── Capability flags ──
    /// Whether the model supports tool/function calling
    #[serde(default = "default_true")]
    pub supports_tool_calling: bool,
    /// Whether the model supports reasoning/thinking (e.g. o1, deepseek-reasoner)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Whether the model supports file attachments (multimodal input)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_attachment: Option<bool>,
    /// Whether the model supports temperature parameter
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,

    // ── Cost (for budget tracking) ──
    /// Pricing information (USD per 1M tokens)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCostInfo>,

    // ── Modalities (for future multimodal support) ──
    /// Supported input/output modalities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelModalities>,

    // ── Metadata (for display and routing) ──
    /// Model display name (e.g. "GPT-4o", "Claude Sonnet 4")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Model family (e.g. "gpt", "claude", "qwen")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Knowledge cutoff date (e.g. "2025-04", "2024-10")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,
}

/// Context usage info reported by Runtime to Gateway after each LLM call.
/// Forwarded to Desktop App via WebSocket for UI display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUsageInfo {
    /// Context window limit (from model capabilities)
    pub context_window: u64,
    /// Current input tokens used (prompt_tokens from API response)
    pub input_tokens: u64,
    /// Current output tokens generated (completion_tokens)
    pub output_tokens: u64,
    /// Total tokens (input + output)
    pub total_tokens: u64,
    /// Max input tokens (from models.dev limit.input, if available)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Usable context space (context_window - max_output_tokens, or max_input_tokens - reserved)
    pub usable_context: u64,
    /// Usage percentage (0-100)
    pub usage_percent: u8,
}

/// LLM API protocol type, derived from models.dev npm field.
///
/// Used by Gateway to tell Runtime which protocol adapter to use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    /// Anthropic Messages API (used by providers with npm: @ai-sdk/anthropic)
    Anthropic,
    /// Ollama native API
    Ollama,
    /// OpenAI-compatible Chat Completions API (default for all other providers)
    #[default]
    #[serde(alias = "openai-compatible")]
    OpenAI,
}

impl std::str::FromStr for ProtocolType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(ProtocolType::Anthropic),
            "ollama" => Ok(ProtocolType::Ollama),
            "openai" | "openai-compatible" => Ok(ProtocolType::OpenAI),
            _ => Err(format!("Unknown protocol type: {}", s)),
        }
    }
}

/// Gateway Service API request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GatewayRequest {
    /// Request an API key for a specific provider
    KeyRelease { provider: String },
    /// Send an Intent to another Agent
    IntentSend {
        target: String,
        action: String,
        params: Value,
        #[serde(rename = "async")]
        async_: bool,
    },
    /// Query remaining budget for a provider
    BudgetQuery { provider: String },
    /// Report token usage
    UsageReport(UsageReport),
    /// Acquire a rate limit token
    RateAcquire { provider: String },
    /// Query identity fields from System Agent
    IdentityQuery { fields: Vec<String> },
    /// Query capabilities for a specific agent or all agents
    CapabilityQuery {
        /// Optional agent ID filter (None = all agents)
        agent_id: Option<String>,
    },
    /// Register a cron entry (S3.4, S5.8 enhanced)
    CronRegister {
        /// Agent ID that owns this cron entry
        agent_id: String,
        /// Cron schedule expression (5-field)
        schedule: String,
        /// Action to fire when the schedule triggers
        action: String,
        /// Params to include in the IntentReceived
        params: Value,
        /// Timezone for schedule interpretation (None = UTC, Some("local") = system local)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// Max retry count on failure (0 = no retry, default 0)
        #[serde(default)]
        retry_count: u32,
        /// Retry backoff interval in seconds (default 60)
        #[serde(default = "default_retry_interval")]
        retry_interval_secs: u64,
        /// Max total executions (None = unlimited)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_runs: Option<u32>,
        /// Expiry timestamp in Unix millis (None = never expires)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
    },
    /// Unregister a cron entry (S3.4)
    CronUnregister {
        /// Cron entry ID to remove
        cron_id: String,
    },
    /// List cron entries for the calling agent (S3.4)
    CronList {},
    /// Runtime reports context usage to Gateway (after each LLM call)
    ContextUsageReport {
        agent_id: String,
        context: ContextUsageInfo,
    },
    /// Agent registration — first message sent after IPC connection
    /// Runtime sends this to identify itself to the Gateway
    AgentHello {
        /// The agent's reverse-domain identifier
        agent_id: String,
        /// The agent's version
        version: String,
        /// Connection role — "main" for the primary IPC connection,
        /// "chunk-relay" for the streaming chunk relay connection.
        /// The Gateway uses this to route IntentReceived only to "main" connections.
        /// Defaults to "main" when absent (backward compatible).
        #[serde(default = "default_connection_role")]
        connection_role: String,
    },
    /// List sessions request (S1.14)
    ///
    /// Runtime sends this to Gateway to request a list of
    /// conversation sessions. Gateway responds with SessionList.
    ListSessions,
    /// Get session messages request (S1.14)
    ///
    /// Runtime sends this to Gateway to request paginated messages
    /// for a specific session. Gateway responds with SessionMessages.
    GetSessionMessages {
        /// Session identifier to query
        session_id: String,
        /// Cursor for pagination (message ID of the last seen message)
        #[serde(skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        /// Maximum number of messages to return
        limit: u32,
        /// Pagination direction: "forward" or "backward"
        direction: String,
    },
    /// Create session request (S1.14)
    ///
    /// Runtime sends this to Gateway to signal that a new
    /// conversation session has been created. Gateway responds
    /// with SessionCreated.
    CreateSession,
    /// Get current session ID request (S1.14)
    ///
    /// Runtime sends this to Gateway to query the currently
    /// active session ID. Gateway responds with CurrentSessionId.
    GetCurrentSessionId,
    /// Delete session request
    ///
    /// Gateway sends this to Runtime to delete a conversation
    /// session. Runtime deletes the JSONL file and responds
    /// with SessionDeleted.
    DeleteSession {
        /// Session identifier to delete
        session_id: String,
    },
}

/// Gateway Service API response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum GatewayResponse {
    /// AgentHello response — confirms registration and delivers all
    /// handshake-time configuration in a single atomic message.
    ///
    /// Bundles LLM config, workspace context, and runtime overrides
    /// so the Runtime does not need to selectively read from the shared
    /// push channel during startup (eliminating the message-loss race).
    AgentHelloResult {
        /// Whether the registration was successful
        success: bool,
        /// Error message if registration failed
        error: Option<String>,

        // ── LLM Configuration (only for "main" connections) ──
        /// Provider name (e.g. "openai", "anthropic")
        provider: Option<String>,
        /// Selected model name
        model: Option<String>,
        /// Decrypted API key from Vault
        api_key: Option<String>,
        /// Custom base URL (if configured)
        base_url: Option<String>,
        /// Available models for this provider
        models: Vec<String>,
        /// Resolved model capabilities (context window, tool calling, etc.)
        model_capabilities: Option<ModelCapabilitiesInfo>,
        /// Gateway-level max output tokens limit
        max_output_tokens_limit: u64,
        /// Resolved protocol type (openai / anthropic / ollama)
        protocol_type: ProtocolType,

        // ── Workspace Context ──
        /// Formatted workspace directory listing for system prompt injection
        workspace_context_text: Option<String>,
        /// ID of the currently-selected workspace (if any)
        current_workspace_id: Option<String>,
        /// Absolute path of the currently-selected workspace (if any)
        current_workspace_path: Option<String>,

        // ── Runtime Config Overrides ──
        /// Per-agent max_output_tokens override
        runtime_max_output_tokens: Option<u64>,
        /// Per-agent max_iterations override
        runtime_max_iterations: Option<u32>,
        /// Per-agent temperature override
        runtime_temperature: Option<f32>,
        /// Per-agent system prompt override
        runtime_system_prompt_override: Option<String>,
        /// Per-agent shell approval threshold.
        /// "low" | "medium" (default) | "high" | "never"
        runtime_shell_approval_threshold: Option<String>,
    },
    /// API key release result
    KeyReleaseResult {
        /// The released API key on success
        api_key: Option<String>,
        /// Error message on failure (e.g. "unauthenticated session", vault error)
        error: Option<String>,
    },
    /// Intent delivery confirmation
    IntentDelivered { message_id: String },
    /// Intent received from another Agent
    IntentReceived {
        from: String,
        action: String,
        params: Value,
        /// Skill command selected by the user (e.g. "/commit", "/review-pr").
        /// When present, the Runtime knows the user explicitly chose a skill.
        /// None for normal chat messages or non-skill intents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// Budget information
    BudgetInfo {
        remaining_tokens: u64,
        remaining_cost_usd: f64,
    },
    /// Usage report acknowledgment
    UsageReportAck {},
    /// Context usage report acknowledgment
    ContextUsageAck {},
    /// Rate limit token
    RateToken {
        granted: bool,
        retry_after_ms: Option<u64>,
    },
    /// Identity delivery (Gateway → Runtime, cold-start injection)
    IdentityDelivery {
        /// List of identity entries from System Agent
        entries: Vec<IdentityEntry>,
    },
    /// LLM configuration delivery (Gateway → Runtime, handshake)
    ///
    /// After AgentHello, Gateway pushes the user's configured LLM provider
    /// to the Agent Runtime. This satisfies PRD GTW-05 and SEC-07:
    /// API keys are distributed via IPC, not environment variables.
    ///
    /// The provider always overrides the manifest's suggested_provider.
    /// model=None means Gateway has no model preference — Runtime falls back
    /// to the manifest's suggested_model.
    LLMConfigDelivery {
        /// Provider name (e.g. "minimax", "openai", "anthropic")
        provider: String,
        /// Model identifier (e.g. "MiniMax-M2.7", "minimax-m2.5").
        /// None when Gateway has no model preference — Runtime uses manifest's suggested_model.
        model: Option<String>,
        /// API key for the provider (one-time delivery, not stored on disk by Runtime)
        api_key: String,
        /// Base URL override (optional, provider-specific)
        base_url: Option<String>,
        /// Available models for this provider (user-selected from models.dev).
        /// The agent can switch between these models at runtime.
        models: Vec<String>,
        /// Model capabilities (context_window, max_output_tokens, tool_calling).
        /// Populated by Gateway from models.dev / offline data.
        /// None when model capabilities are not available (e.g. unknown model).
        #[serde(default)]
        model_capabilities: Option<ModelCapabilitiesInfo>,
        /// Global max output tokens limit (from Gateway config).
        /// When set, this value caps the max_output_tokens used in API requests
        /// and context usage calculations, overriding model capabilities if they exceed it.
        /// Default: 32768 (32K). Set to 0 to disable the limit.
        #[serde(default = "default_max_output_tokens_limit")]
        max_output_tokens_limit: u64,
        /// Protocol type for the LLM API (anthropic/openai/ollama)
        #[serde(default)]
        protocol_type: ProtocolType,
    },
    /// Identity query result from System Agent
    IdentityQueryResult {
        /// Field values
        values: std::collections::HashMap<String, String>,
        /// Confidence scores per field
        confidence: std::collections::HashMap<String, f32>,
    },
    /// Capability overview (handshake step ⑤ and CapabilityQuery response)
    CapabilityOverview {
        /// Map of agent_id → list of action names
        capabilities: std::collections::HashMap<String, Vec<String>>,
    },
    /// Capability update (incremental push on install/uninstall/update)
    CapabilityUpdate {
        /// Agent that was updated
        agent_id: String,
        /// New/updated actions
        actions: Vec<String>,
        /// Whether this is a removal
        removed: bool,
    },
    /// Cron registration result (S3.4)
    CronRegisterResult {
        /// Cron entry ID on success
        cron_id: Option<String>,
        /// Error message on failure
        error: Option<String>,
    },
    /// Cron unregistration result (S3.4)
    CronUnregisterResult {
        /// Whether the entry was found and removed
        removed: bool,
    },
    /// Cron list result (S3.4)
    CronListResult {
        /// List of cron entries
        entries: Vec<CronEntryInfo>,
    },
    /// Workspace context update (Gateway → Runtime, push)
    ///
    /// Pushes the formatted workspace context text to the Agent Runtime
    /// so it can inject it into the LLM system prompt. Sent at two times:
    ///   A) After AgentHello handshake (initial workspace config)
    ///   B) When the user switches the current workspace (hot update)
    WorkspaceContextUpdate {
        /// Formatted workspace context text (Markdown, ready for LLM injection)
        context_text: String,
        /// ID of the currently selected workspace (if any)
        current_workspace_id: Option<String>,
        /// Absolute path of the currently selected workspace (if any)
        current_workspace_path: Option<String>,
    },
    /// Iteration limit reached — agent loop paused, awaiting user decision.
    ///
    /// The Runtime pushes this when `iteration >= max_iterations`.
    /// The Gateway relays it to the Desktop App so the user can choose
    /// to continue (which resets the iteration counter) or stop.
    IterationLimitPaused {
        /// Current iteration count when the limit was hit
        iteration: u32,
        /// Configured max_iterations limit
        max_iterations: u32,
        /// Human-readable message
        message: String,
    },
    /// Session list result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::ListSessions.
    /// Carries the list of session summaries.
    SessionList {
        /// List of session info DTOs
        sessions: Vec<SessionInfoDto>,
    },
    /// Session messages result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::GetSessionMessages.
    /// Carries a paginated page of conversation messages.
    SessionMessages {
        /// Messages in the current page
        messages: Vec<ConversationEntryDto>,
        /// Cursor for the next page (message ID)
        cursor: Option<String>,
        /// Whether more messages exist beyond this page
        has_more: bool,
    },
    /// Session created result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::CreateSession.
    SessionCreated {
        /// The newly created session identifier
        session_id: String,
    },
    /// Current session ID result (S1.14)
    ///
    /// Sent by Gateway in response to GatewayRequest::GetCurrentSessionId.
    CurrentSessionId {
        /// The currently active session ID, or None if no session
        session_id: Option<String>,
    },
    /// Session deleted result
    ///
    /// Sent by Runtime in response to GatewayRequest::DeleteSession.
    SessionDeleted {
        /// Whether the session was successfully deleted
        success: bool,
        /// Error message if deletion failed
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Log level update (Gateway → Runtime, push)
    ///
    /// Gateway pushes a new log level when the user changes it in Settings.
    /// The Runtime applies the change to its tracing subscriber via reload::Handle.
    LogLevelUpdate {
        /// New log level string (e.g. "trace", "debug", "info", "warn", "error")
        log_level: String,
    },
    /// Log rotation request (Gateway → Runtime, push)
    ///
    /// Gateway pushes this when the user triggers log cleanup in Settings.
    /// The Runtime must:
    ///   1. Delete all *.log files in its workspace/logs/ directory
    ///   2. Force-rotate to create a fresh log file for subsequent writes
    LogRotate,
    /// Runtime configuration update (Gateway → Runtime, push)
    ///
    /// Gateway pushes per-agent config overrides to the Runtime.
    /// Sent at two times:
    ///   A) After AgentHello handshake (initial config delivery)
    ///   B) When the user updates config via PUT /api/agents/{id}/config
    ///
    /// All fields are optional — None means "keep current value".
    RuntimeConfigUpdate {
        /// Max output tokens per request (0 = use global default)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u64>,
        /// Max LLM iterations per run (0 = use global default).
        /// Controls the total number of LLM turns in a single Agent loop.
        /// When exceeded, the Runtime pushes `IterationLimitPaused`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
        /// LLM temperature override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        /// System prompt override
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt_override: Option<String>,
        /// Active tool names (overrides manifest [[tools]] declarations).
        /// Some(vec![]) means no tools active; None means keep current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_tools: Option<Vec<String>>,
        /// Shell command approval threshold.
        /// Controls which risk levels require user confirmation before execution.
        /// "low" | "medium" (default) | "high" | "never"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell_approval_threshold: Option<String>,
    },
    /// Unknown or unrecognized message from Gateway.
    ///
    /// Returned when proto_to_gateway_response encounters an empty payload
    /// or an unrecognized variant. This is distinct from normal business
    /// messages so the agent loop can log and discard it without confusing
    /// it with a legitimate UsageReportAck or other response.
    Unknown {},
}

/// Session info DTO for IPC responses (S1.14)
///
/// Carries session metadata from Runtime to Gateway
/// so the HTTP API can return session lists without
/// directly reading JSONL files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfoDto {
    /// Session identifier (e.g. "20260503_143022_a1b2c3")
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Number of messages in the session
    pub message_count: u32,
    /// Optional session title
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the session metadata was recovered from a corrupted first line
    #[serde(default)]
    pub corrupted: bool,
}

/// Conversation entry DTO for IPC responses (S1.14)
///
/// Carries a single message from Runtime to Gateway
/// for paginated message queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntryDto {
    /// Unique message ID
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// Message role: "user" | "assistant" | "think" | "tool_call" | "tool_result" | "system"
    pub role: String,
    /// Full message content
    pub content: String,
    /// Optional metadata (e.g. tool_call_id, tool_name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Available tool information returned by GET /api/agents/{id}/tools.
///
/// Describes a built-in tool that the agent can activate, including
/// its name, description, and the permissions required to use it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableTool {
    /// Tool name (e.g. "file_read", "http_request")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Permission strings required to use this tool
    /// (e.g. ["filesystem:read:<path>", "network:<url>"])
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_permissions: Vec<String>,
}

/// Response for GET /api/agents/{id}/tools.
///
/// Returns all available built-in tools plus the current active set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableToolsResponse {
    pub agent_id: String,
    /// All available built-in tools
    pub tools: Vec<AvailableTool>,
    /// Currently active tool names (from manifest + config overrides)
    pub active_tools: Vec<String>,
}

/// Cron entry info (for IPC responses, S5.8 enhanced)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntryInfo {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent ID that owns this entry
    pub agent_id: String,
    /// Cron schedule expression
    pub schedule: String,
    /// Action to fire
    pub action: String,
    /// Params for the IntentReceived
    pub params: Value,
    /// Timezone for schedule interpretation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Max retry count on failure
    #[serde(default)]
    pub retry_count: u32,
    /// Retry backoff interval in seconds
    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,
    /// Max total executions (None = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u32>,
    /// Current execution count
    #[serde(default)]
    pub run_count: u32,
    /// Expiry timestamp in Unix millis
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// Default retry interval: 60 seconds
fn default_retry_interval() -> u64 {
    60
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_request_serialize_key_release() {
        let req = GatewayRequest::KeyRelease {
            provider: "openai".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"KeyRelease\""));
        assert!(json.contains("\"provider\":\"openai\""));
    }

    #[test]
    fn test_gateway_request_roundtrip() {
        let req = GatewayRequest::IntentSend {
            target: "com.example.calendar".into(),
            action: "schedule".into(),
            params: serde_json::json!({"time": "10:00"}),
            async_: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: GatewayRequest = serde_json::from_str(&json).unwrap();
        if let GatewayRequest::IntentSend {
            target, action, ..
        } = parsed
        {
            assert_eq!(target, "com.example.calendar");
            assert_eq!(action, "schedule");
        } else {
            panic!("Expected IntentSend variant");
        }
    }

    #[test]
    fn test_gateway_response_roundtrip() {
        let resp = GatewayResponse::BudgetInfo {
            remaining_tokens: 50000,
            remaining_cost_usd: 1.5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::BudgetInfo {
            remaining_tokens, ..
        } = parsed
        {
            assert_eq!(remaining_tokens, 50000);
        } else {
            panic!("Expected BudgetInfo variant");
        }
    }

    #[test]
    fn test_intent_received_without_command() {
        let resp = GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "chat_message".to_string(),
            params: serde_json::json!({"content": "hello"}),
            command: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        // command should be skipped when None
        assert!(!json.contains("command"));
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::IntentReceived { from, action, command, .. } = parsed {
            assert_eq!(from, "http-api");
            assert_eq!(action, "chat_message");
            assert!(command.is_none());
        } else {
            panic!("Expected IntentReceived variant");
        }
    }

    #[test]
    fn test_intent_received_with_command() {
        let resp = GatewayResponse::IntentReceived {
            from: "http-api".to_string(),
            action: "chat_message".to_string(),
            params: serde_json::json!({"content": "hello"}),
            command: Some("/commit".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("command"));
        let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();
        if let GatewayResponse::IntentReceived { from, action, command, .. } = parsed {
            assert_eq!(from, "http-api");
            assert_eq!(action, "chat_message");
            assert_eq!(command, Some("/commit".to_string()));
        } else {
            panic!("Expected IntentReceived variant");
        }
    }

    #[test]
    fn test_intent_received_backward_compatible() {
        // Old JSON without command field should deserialize with command=None
        let json = r#"{"type":"IntentReceived","from":"http-api","action":"chat_message","params":{"content":"hello"}}"#;
        let parsed: GatewayResponse = serde_json::from_str(json).unwrap();
        if let GatewayResponse::IntentReceived { command, .. } = parsed {
            assert!(command.is_none());
        } else {
            panic!("Expected IntentReceived variant");
        }
    }



}
