//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::sync::Arc;

use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::protocol::ProtocolType;
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
    /// Model override from Gateway (takes precedence over manifest's suggested_model)
    pub override_model: Option<String>,
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
            override_model: None,
        }
    }
}

/// Accumulated runtime config overrides pushed by Gateway via
/// `RuntimeConfigUpdate`. Applied on top of the shared `AgentCore` template
/// each time a new session is spawned, so config changes remain effective
/// for sessions created *after* the push (not only for sessions that were
/// already alive when the push arrived).
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfigOverrides {
    pub max_output_tokens: Option<u64>,
    pub max_iterations: Option<u32>,
    pub temperature: Option<f32>,
    pub system_prompt_override: Option<String>,
}

impl RuntimeConfigOverrides {
    /// Returns true when no override value has been set.
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.temperature.is_none()
            && self.system_prompt_override.is_none()
    }

    /// Merge in a newer push. `Some` values replace; `None` preserves the
    /// previously cached override.
    pub fn merge(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
    ) {
        if max_output_tokens.is_some() {
            self.max_output_tokens = max_output_tokens;
        }
        if max_iterations.is_some() {
            self.max_iterations = max_iterations;
        }
        if temperature.is_some() {
            self.temperature = temperature;
        }
        if system_prompt_override.is_some() {
            self.system_prompt_override = system_prompt_override;
        }
    }
}

/// Cached LLM configuration from the latest Gateway LLMConfigDelivery push.
///
/// Stored so that sessions created *after* a model/provider switch inherit
/// the correct provider (via `UpdateProvider`) and capabilities, rather
/// than falling back to the stale values in the `AgentCore` template.
#[derive(Debug, Clone)]
struct CachedLLMConfig {
    provider_name: String,
    protocol_type: ProtocolType,
    api_key: String,
    base_url: Option<String>,
    model: String,
    capabilities: Option<ModelCapabilitiesInfo>,
    max_output_tokens_limit: u64,
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
    /// Runtime config overrides (accumulated from Gateway pushes) that
    /// must be re-applied to every newly created session.
    runtime_overrides: RuntimeConfigOverrides,
    /// Cached workspace context (from AgentHello or Gateway push) that
    /// must be re-applied to every newly created session.
    workspace_context: Option<String>,
    /// Cached LLM config from LLMConfigDelivery (provider params, caps, limit)
    /// that must be re-applied to every newly created session.
    cached_llm: Option<CachedLLMConfig>,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        Self {
            core,
            sessions: HashMap::new(),
            config,
            runtime_overrides: RuntimeConfigOverrides::default(),
            workspace_context: None,
            cached_llm: None,
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

        let (task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.tool_definitions.clone(),
            self.config.identity_context.clone(),
            self.config.override_model.clone(),
        );

        // Spawn the session task with panic isolation
        let join_handle = tokio::spawn(async move {
            task.run().await;
        });

        let handle = SessionHandle {
            session_id: session_id.clone(),
            inbound_tx,
            agent_inbound_tx,
            join_handle,
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        // Re-apply any runtime config overrides accumulated from prior
        // Gateway pushes. Without this, a new session would start from the
        // immutable `Arc<AgentCore>` template (e.g. default `max_iterations`
        // of 50) and ignore values the user has already applied in the UI.
        if !self.runtime_overrides.is_empty() {
            let ov = self.runtime_overrides.clone();
            tracing::info!(
                session_id = %session_id,
                max_output_tokens = ?ov.max_output_tokens,
                max_iterations = ?ov.max_iterations,
                temperature = ?ov.temperature,
                "SessionManager: replaying RuntimeConfigOverrides to new session"
            );
            // Safe: the handle was just inserted above.
            if let Some(handle) = self.sessions.get(&session_id) {
                let _ = handle.send(SessionMessage::UpdateRuntimeConfig {
                    max_output_tokens: ov.max_output_tokens,
                    max_iterations: ov.max_iterations,
                    temperature: ov.temperature,
                    system_prompt_override: ov.system_prompt_override,
                });
            }
        }

        // Re-apply the cached workspace context to the new session.
        // This is separate from `runtime_overrides` because workspace
        // context is a large string (not a config override) and follows
        // the same cache-and-replay pattern.
        if let Some(ref ctx) = self.workspace_context {
            tracing::info!(
                session_id = %session_id,
                ctx_len = ctx.len(),
                "SessionManager: replaying workspace context to new session"
            );
            if let Some(handle) = self.sessions.get(&session_id) {
                let _ = handle.send(SessionMessage::UpdateWorkspaceContext {
                    context_text: ctx.clone(),
                });
            }
        }

        // Re-apply the cached LLM config (provider params, capabilities,
        // max_output_tokens) to the new session. This mirrors the
        // RuntimeConfigOverrides replay pattern for consistency.
        if let Some(ref cached) = self.cached_llm {
            tracing::info!(
                session_id = %session_id,
                provider = %cached.provider_name,
                model = %cached.model,
                "SessionManager: replaying LLM config to new session"
            );
            if let Some(handle) = self.sessions.get(&session_id) {
                let _ = handle.send(SessionMessage::UpdateProvider {
                    provider_name: cached.provider_name.clone(),
                    protocol_type: cached.protocol_type.clone(),
                    api_key: Some(cached.api_key.clone()),
                    base_url: cached.base_url.clone(),
                    model: cached.model.clone(),
                });
                if let Some(ref caps) = cached.capabilities {
                    let _ = handle.send(SessionMessage::UpdateCapabilities {
                        caps: caps.clone(),
                    });
                }
                let _ = handle.send(SessionMessage::UpdateMaxOutputTokens {
                    limit: cached.max_output_tokens_limit,
                });
            }
        }

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

    /// Apply a runtime config override pushed by Gateway.
    ///
    /// This performs two actions atomically from the caller's perspective:
    ///   1. Merge the override into the `runtime_overrides` cache so any
    ///      session created *after* this call also picks it up (fixing the
    ///      bug where a fresh session would clone the untouched
    ///      `Arc<AgentCore>` template and silently ignore user-applied
    ///      values such as `max_iterations`).
    ///   2. Broadcast the override to all currently active sessions.
    pub fn apply_runtime_config_override(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
    ) -> Vec<String> {
        self.runtime_overrides.merge(
            max_output_tokens,
            max_iterations,
            temperature,
            system_prompt_override.clone(),
        );
        self.broadcast(SessionMessage::UpdateRuntimeConfig {
            max_output_tokens,
            max_iterations,
            temperature,
            system_prompt_override,
        })
    }

    /// Cache LLM config (provider, capabilities, limit) from LLMConfigDelivery
    /// and broadcast to all active sessions.
    ///
    /// Follows the same cache+broadcast pattern: the config is cached so
    /// sessions created *after* a model switch inherit the correct provider,
    /// capabilities, and token limits.
    #[allow(clippy::too_many_arguments)]
    pub fn update_llm_config(
        &mut self,
        provider_name: String,
        protocol_type: ProtocolType,
        api_key: String,
        base_url: Option<String>,
        model: String,
        capabilities: Option<ModelCapabilitiesInfo>,
        max_output_tokens_limit: u64,
    ) -> Vec<String> {
        tracing::info!(
            provider = %provider_name,
            model = %model,
            max_output_tokens_limit = max_output_tokens_limit,
            "SessionManager: caching LLM config"
        );
        self.cached_llm = Some(CachedLLMConfig {
            provider_name: provider_name.clone(),
            protocol_type: protocol_type.clone(),
            api_key: api_key.clone(),
            base_url: base_url.clone(),
            model: model.clone(),
            capabilities: capabilities.clone(),
            max_output_tokens_limit,
        });

        // Broadcast to existing sessions (matching broadcast() pattern:
        // iterate &self.sessions directly to avoid active_sessions() allocation
        // and send_to_session() double-lookup).
        let mut failed = Vec::new();
        for (sid, handle) in &self.sessions {
            if handle.send(SessionMessage::UpdateProvider {
                provider_name: provider_name.clone(),
                protocol_type: protocol_type.clone(),
                api_key: Some(api_key.clone()),
                base_url: base_url.clone(),
                model: model.clone(),
            }).is_err() {
                failed.push(sid.clone());
            }
            if let Some(ref caps) = capabilities {
                if handle.send(SessionMessage::UpdateCapabilities {
                    caps: caps.clone(),
                }).is_err() {
                    if !failed.contains(sid) {
                        failed.push(sid.clone());
                    }
                }
            }
            if handle.send(SessionMessage::UpdateMaxOutputTokens {
                limit: max_output_tokens_limit,
            }).is_err() {
                if !failed.contains(sid) {
                    failed.push(sid.clone());
                }
            }
        }
        failed
    }

    /// Cache workspace context and broadcast to all active sessions.
    ///
    /// This mirrors `apply_runtime_config_override`: the context is
    /// cached so any session created *after* this call also receives
    /// it (fixing the bug where a fresh session after deletion would
    /// lose its workspace context).
    pub fn set_workspace_context(&mut self, context_text: String) -> Vec<String> {
        tracing::info!(
            ctx_len = context_text.len(),
            "SessionManager: caching workspace context"
        );
        self.workspace_context = Some(context_text.clone());
        self.broadcast(SessionMessage::UpdateWorkspaceContext {
            context_text,
        })
    }

    /// Update model override and broadcast to all active sessions.
    ///
    /// Follows the same cache+broadcast pattern as other ambient state:
    /// the model is stored in `SessionManagerConfig.override_model` so that
    /// sessions created *after* this call inherit the latest model, while
    /// existing sessions receive the update via broadcast.
    pub fn update_model_override(&mut self, model: String) -> Vec<String> {
        tracing::info!(
            model = %model,
            "SessionManager: caching model override"
        );
        self.config.override_model = Some(model.clone());
        self.broadcast(SessionMessage::ModelSwitch {
            model,
        })
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

    /// Access the Grafeo memory store from the shared core.
    /// Returns None if the memory store was not initialized.
    pub(crate) fn memory_store(&self) -> Option<&Arc<rollball_grafeo::grafeo::GrafeoStore>> {
        self.core.memory_store()
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
