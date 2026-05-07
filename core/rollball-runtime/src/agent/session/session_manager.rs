//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::sync::Arc;

use rollball_core::Budget;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_core::AgentCore;
use crate::agent::loop_::ChunkEvent;
use crate::agent::session::session_handle::SessionHandle;
use crate::agent::session::session_task::{SessionMessage, SessionTask};
use crate::agent::session_state::SessionState;
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};

/// Configuration for SessionManager.
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Channel capacity for each session's inbound message queue
    pub inbound_channel_capacity: usize,
    /// System prompt to use for all sessions
    pub system_prompt: String,
    /// Per-session token budget
    pub per_session_budget: Budget,
    /// History max tokens per session
    pub history_max_tokens: u64,
    /// Number of full tool results to keep per session
    pub keep_full_results: usize,
    /// Optional streaming chunk sender shared across all sessions.
    /// When set, each session's AgentLoop forwards ChunkEvents here
    /// so the caller can relay them to Gateway.
    pub chunk_tx: Option<mpsc::Sender<ChunkEvent>>,
    /// Complete tool definitions (with input_schema) for ContextBuilder.
    /// SessionTask uses these instead of building simplified ones from manifest.
    pub tool_definitions: Vec<serde_json::Value>,
    /// Identity context string injected by Gateway for ContextBuilder.
    pub identity_context: Option<String>,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            inbound_channel_capacity: 64,
            system_prompt: String::new(),
            per_session_budget: Budget {
                daily_tokens: None,
                monthly_tokens: None,
                daily_cost_usd: None,
                monthly_cost_usd: None,
                exceeded_action: "warn".to_string(),
            },
            history_max_tokens: 128_000,
            keep_full_results: 4,
            chunk_tx: None,
            tool_definitions: Vec::new(),
            identity_context: None,
        }
    }
}

/// Lifecycle manager for multiple concurrent sessions.
///
/// Owns a shared `Arc<AgentCore>` template and creates `SessionTask`s
/// on demand. Each session gets an independent `SessionState` while
/// sharing the provider, tools, and config from the core template.
pub struct SessionManager {
    /// Shared agent core template for cloning into sessions
    core: Arc<AgentCore>,
    /// Active session handles, keyed by session ID
    sessions: HashMap<String, SessionHandle>,
    /// Configuration for session creation
    config: SessionManagerConfig,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        Self {
            core,
            sessions: HashMap::new(),
            config,
        }
    }

    /// Create a new session, spawning it as an independent tokio task.
    ///
    /// Returns the session ID on success.
    pub async fn create_session(&mut self) -> Result<String> {
        let session_id = Uuid::new_v4().to_string();
        self.create_session_with_id(session_id).await
    }

    /// Create a new session with a specific ID.
    ///
    /// Useful for testing or when the session ID needs to be deterministic.
    pub async fn create_session_with_id(&mut self, session_id: String) -> Result<String> {
        self.create_session_with_id_and_conversation(session_id, None).await
    }

    /// Create a new session with a specific ID and optional conversation session.
    ///
    /// When `conversation` is provided, the session is initialized with JSONL
    /// persistence enabled. This is used for the initial session on cold start
    /// when a previous conversation is resumed.
    pub async fn create_session_with_id_and_conversation(
        &mut self,
        session_id: String,
        conversation: Option<ConversationSession>,
    ) -> Result<String> {
        let (inbound_tx, inbound_rx) =
            mpsc::channel(self.config.inbound_channel_capacity);

        let session_state = SessionState::new(
            self.config.history_max_tokens,
            self.config.keep_full_results,
            self.config.per_session_budget.clone(),
            conversation,
        );

        let task = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.tool_definitions.clone(),
            self.config.identity_context.clone(),
        );

        // Spawn the session task with panic isolation
        let join_handle = tokio::spawn(async move {
            task.run().await;
        });

        let handle = SessionHandle {
            session_id: session_id.clone(),
            inbound_tx,
            join_handle,
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        Ok(session_id)
    }

    /// Destroy a session by ID, sending a Stop message and removing it.
    ///
    /// Returns an error if the session does not exist.
    pub async fn destroy_session(&mut self, session_id: &str) -> Result<()> {
        let handle = self.sessions.remove(session_id).ok_or_else(|| {
            RuntimeError::Config(format!("Session not found: {}", session_id))
        })?;

        // Send Stop signal; ignore errors (session may have already stopped)
        let _ = handle.inbound_tx.send(SessionMessage::Stop).await;
        tracing::info!(session_id = %session_id, "SessionManager: destroyed session");
        Ok(())
    }

    /// Send a message to a specific session.
    ///
    /// Returns an error if the session does not exist or the channel is closed.
    pub fn send_to_session(
        &self,
        session_id: &str,
        msg: SessionMessage,
    ) -> Result<()> {
        let handle = self.sessions.get(session_id).ok_or_else(|| {
            RuntimeError::Config(format!("Session not found: {}", session_id))
        })?;
        handle.send(msg).map_err(|_| {
            RuntimeError::Config(format!(
                "Failed to send message to session {}: channel closed",
                session_id
            ))
        })
    }

    /// Broadcast a message to all active sessions.
    ///
    /// Returns a list of session IDs that failed to receive the message
    /// (e.g., because the channel was closed).
    pub fn broadcast(&self, msg: SessionMessage) -> Vec<String> {
        let mut failed = Vec::new();
        for (session_id, handle) in &self.sessions {
            if handle.send(msg.clone()).is_err() {
                failed.push(session_id.clone());
            }
        }
        if !failed.is_empty() {
            tracing::warn!(
                failed_count = failed.len(),
                "Broadcast failed for some sessions"
            );
        }
        failed
    }

    /// Get all active session IDs.
    pub fn active_sessions(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Look up a session handle by ID.
    pub fn get_session(&self, session_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(session_id)
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get the suggested provider name from the shared core manifest.
    /// Used for budget queries in the Gateway loop.
    pub fn provider_name(&self) -> String {
        self.core.manifest().llm.suggested_provider.clone()
    }

    /// Reap completed sessions (remove handles for tasks that have finished).
    ///
    /// Call this periodically to avoid memory leaks from accumulated
    /// JoinHandle values for completed sessions.
    pub fn reap_finished(&mut self) {
        let finished: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, h)| h.join_handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();

        for id in finished {
            tracing::debug!(session_id = %id, "Reaping finished session handle");
            self.sessions.remove(&id);
        }
    }
}
