//! Session metadata query use case.
//!
//! ADR-040: this trait is the single source of truth for session-listing
//! and message-reading operations. Both the HTTP server and any future
//! transport adapter call through this trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Summary of a single session for list responses (ADR-024 / ADR-028).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u32,
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// Response for `list_sessions` — paginated session list with
/// agent-level cumulative token totals (ADR-028 / ADR-066).
///
/// Cache fields are emitted unconditionally because the runtime
/// always initialises the agent counters (Commit 2 sets both
/// `agent_total_cache_read_tokens` and `agent_total_cache_write_tokens`
/// to `0` on every construction site).  Desktop frontends that do
/// not yet read these fields stay compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionSummary>,
    pub total_count: usize,
    pub total_pages: u32,
    pub page: u32,
    pub size: u32,
    pub agent_total_input_tokens: u64,
    pub agent_total_output_tokens: u64,
    /// ADR-066: cumulative cache-hit tokens (provider-billed as discounted read).
    pub agent_total_cache_read_tokens: u64,
    /// ADR-066: cumulative cache-write tokens (provider-billed as upfront write).
    pub agent_total_cache_write_tokens: u64,
}

/// Detail view of a single session (panel-4 endpoint).
///
/// ADR-047: config fields (model, provider, workspace_id,
/// reasoning_effort, temperature, title) have been moved to
/// `GET /sessions/{sid}/config` (served by `SessionConfigService`).
/// This struct now carries only state + metadata fields.
///
/// `live_state` is an optional JSON object carrying the runtime
/// snapshot fields (status, model, provider, ratio, todos,
/// context_usage). The impl constructs this from
/// `SharedSessionSnapshots`; `None` means no live snapshot exists.
///
/// Note: `model` and `provider` in `live_state` are runtime telemetry
/// (which model the LLM is currently using), not config. Config-level
/// model/provider are in `SessionConfigSnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_state: Option<serde_json::Value>,
}

/// Response for `get_messages` — paginated messages from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub session_id: String,
    pub messages: Vec<serde_json::Value>,
    pub offset: u64,
    pub limit: u32,
    pub total: u64,
    pub count: usize,
}

/// Session metadata query methods.
#[async_trait]
pub trait SessionMetadataService: Send + Sync {
    /// List sessions with pagination and agent-level token totals (ADR-028).
    async fn list_sessions(&self, page: u32, size: u32) -> Result<SessionsListResponse>;

    /// Return the most recently active session, if any.
    async fn get_latest_session(&self) -> Result<Option<(String, Option<String>)>>;

    /// Return a single session's detail (meta + live state).
    async fn get_session(&self, session_id: &str) -> Result<SessionDetail>;

    /// Read messages from a session with offset-based pagination
    /// (ADR-050 forward semantics: `offset = 0` = oldest entry).
    ///
    /// When `from_tail` is `true`, the window is anchored to the last
    /// `limit` entries regardless of `offset` (initial-load path).
    async fn get_messages(
        &self,
        session_id: &str,
        offset: Option<u64>,
        limit: Option<u32>,
        from_tail: bool,
    ) -> Result<MessagesResponse>;
}
