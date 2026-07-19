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
/// agent-level cumulative token totals (ADR-028).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionSummary>,
    pub total_count: usize,
    pub total_pages: u32,
    pub page: u32,
    pub size: u32,
    pub agent_total_input_tokens: u64,
    pub agent_total_output_tokens: u64,
}

/// Detail view of a single session (panel-4 endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u32,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_id: Option<String>,
    // Live state snapshot from SessionManager.
    pub state: Option<String>,
}

/// Response for `get_messages` — paginated messages from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub session_id: String,
    pub messages: Vec<serde_json::Value>,
    pub count: u64,
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

    /// Read messages from a session with offset-based pagination.
    async fn get_messages(
        &self,
        session_id: &str,
        offset: Option<u64>,
        limit: Option<u32>,
    ) -> Result<MessagesResponse>;
}
