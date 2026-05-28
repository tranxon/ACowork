//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::sync::Arc;

use rollball_core::protocol::ModelCapabilitiesInfo;
use rollball_core::protocol::ProtocolType;
use rollball_core::protocol::{SearchKeyEntry, SearchProviderListItem};
use rollball_core::tools::traits::Tool;
use rollball_core::Budget;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::loop_::SessionChunkEvent;
use crate::agent::session::session_handle::SessionHandle;
use crate::agent::session::session_task::{SessionMessage, SessionTask};
use crate::agent::session_state::{SessionState, SessionStatus};
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::tools::mcp_manager::McpManager;
use crate::tools::workspace_resolver::{WorkspaceResolver, format_workspace_context_for_session};

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
    pub chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Complete tool definitions (with input_schema) for ContextBuilder.
    /// SessionTask uses these instead of building simplified ones from manifest.
    pub tool_definitions: Vec<serde_json::Value>,
    /// Full tool specs (name, schema) for ALL registered built-in tools.
    /// Stored so that tool definitions can be hot-rebuilt when `active_tools`
    /// changes without requiring access to the ToolRegistry (which is behind Arc).
    pub full_tool_specs: Vec<(String, serde_json::Value)>,
    /// Identity context string injected by Gateway for ContextBuilder.
    pub identity_context: Option<String>,
    /// Model override from Gateway (takes precedence over manifest's suggested_model)
    pub override_model: Option<String>,
    /// LLM protocol type derived from models.dev (used for image token estimation)
    pub protocol_type: ProtocolType,
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
            full_tool_specs: Vec::new(),
            identity_context: None,
            override_model: None,
            protocol_type: ProtocolType::default(),
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
    pub active_tools: Option<Vec<String>>,
    pub shell_approval_threshold: Option<String>,
}

impl RuntimeConfigOverrides {
    /// Returns true when no override value has been set.
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.temperature.is_none()
            && self.system_prompt_override.is_none()
            && self.active_tools.is_none()
            && self.shell_approval_threshold.is_none()
    }

    /// Merge in a newer push. `Some` values replace; `None` preserves the
    /// previously cached override.
    pub fn merge(
        &mut self,
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        active_tools: Option<Vec<String>>,
        shell_approval_threshold: Option<String>,
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
        if active_tools.is_some() {
            self.active_tools = active_tools;
        }
        if shell_approval_threshold.is_some() {
            self.shell_approval_threshold = shell_approval_threshold;
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

/// Cached search configuration from the latest Gateway SearchConfigDelivery push.
///
/// Mirrors `CachedLLMConfig`: stores search_key_vault (API keys, never persisted)
/// and search provider list (metadata) so that sessions created *after* a search
/// config change can use the latest keys and provider list.
#[derive(Debug, Clone)]
pub(crate) struct CachedSearchConfig {
    pub key_vault: Vec<SearchKeyEntry>,
    pub provider_list: Vec<SearchProviderListItem>,
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
    pub runtime_overrides: RuntimeConfigOverrides,
    /// Cached workspace context (from AgentHello or Gateway push) that
    /// must be re-applied to every newly created session.
    /// After the session-workspace refactor, this is kept for backward
    /// compatibility; new code should use `session_workspaces`.
    workspace_context: Option<String>,
    /// Cached LLM config from LLMConfigDelivery (provider params, caps, limit)
    /// that must be re-applied to every newly created session.
    cached_llm: Option<CachedLLMConfig>,
    /// Cached search config from SearchConfigDelivery (key vault + provider list).
    /// Mirrors `cached_llm` — stored so ConfigSnapshot can return current
    /// search provider metadata for the Agent Setup UI.
    cached_search_config: Option<CachedSearchConfig>,
    /// The currently active session ID — used as the default routing target
    /// when an incoming message does not specify an explicit session_id.
    /// Owned here (not in cli.rs) so SessionManager is the single source of truth.
    current_session_id: String,
    /// MCP tool wrappers, built when MCP servers are connected.
    /// Merged into each new session's tools at creation time.
    mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    /// MCP connection manager.
    mcp_manager: McpManager,
    /// Per-session workspace selection.
    /// Maps session_id → workspace_id (or "__agent_home__" for agent home).
    session_workspaces: HashMap<String, String>,
    /// Per-session pending workspace reference.
    /// When a session's last workspace was deleted from the list,
    /// the session_id → ws_id mapping is moved here so it can be
    /// reconciled if the workspace is re-added.
    pub pending_workspaces: HashMap<String, String>,
    /// Default workspace ID for new sessions (no persisted workspace).
    /// Falls back to "__agent_home__" when no last_active workspace is set.
    default_workspace_id: String,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core, config, and initial session ID.
    ///
    /// The `initial_session_id` is set as the current active session for routing
    /// messages that don't specify an explicit session_id.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig, initial_session_id: String) -> Self {
        Self {
            core,
            sessions: HashMap::new(),
            config,
            runtime_overrides: RuntimeConfigOverrides::default(),
            workspace_context: None,
            cached_llm: None,
            cached_search_config: None,
            current_session_id: initial_session_id,
            mcp_tools: None,
            mcp_manager: McpManager::new(),
            session_workspaces: HashMap::new(),
            pending_workspaces: HashMap::new(),
            default_workspace_id: "__agent_home__".to_string(),
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
        // Read the persisted workspace_id before the conversation is moved
        // into SessionState, so we can restore it to session_workspaces.
        let persisted_workspace_id = conversation
            .as_ref()
            .and_then(|c| c.workspace_id())
            .map(|w| w.to_string());

        let (inbound_tx, inbound_rx) =
            mpsc::channel(self.config.inbound_channel_capacity);

        let session_state = SessionState::new(
            self.config.history_max_tokens,
            self.config.keep_full_results,
            self.config.per_session_budget.clone(),
            conversation,
        );

        let (mut task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.tool_definitions.clone(),
            self.config.identity_context.clone(),
            self.config.override_model.clone(),
            self.config.protocol_type.clone(),
            self.mcp_tools.clone(),
        );

        // ADR-014: Create watch channel for session status
        let (status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        task.set_status_tx(status_tx);

        // Spawn the session task with panic isolation.
        let join_handle = tokio::spawn(async move {
            task.run().await;
        });

        let handle = SessionHandle {
            session_id: session_id.clone(),
            inbound_tx,
            agent_inbound_tx,
            join_handle,
            status_rx,
            last_active_at: std::sync::Mutex::new(std::time::Instant::now()),
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        // Initialize per-session workspace.
        // For resumed sessions, restore the persisted workspace_id from JSONL metadata.
        // New sessions default to last_active workspace (or agent home fallback).
        // Use set_session_workspace() to both update the in-memory map and persist
        // the workspace_id to the session's JSONL conversation file.
        let initial_workspace = persisted_workspace_id.unwrap_or_else(|| self.default_workspace_id.clone());
        self.set_session_workspace(&session_id, &initial_workspace);

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
                    shell_approval_threshold: ov.shell_approval_threshold,
                });
            }
        }

        // Re-apply active tools override to the new session.
        // This ensures sessions created *after* a tools config change
        // inherit the correct tool_definitions.
        if let Some(ref active_tools) = self.runtime_overrides.active_tools {
            let rebuilt = crate::agent::context::build_tool_definitions_from_names(
                active_tools,
                &self.config.full_tool_specs,
            );
            tracing::info!(
                session_id = %session_id,
                tool_count = rebuilt.len(),
                active_tool_names = ?active_tools,
                "SessionManager: replaying active tools to new session"
            );
            if let Some(handle) = self.sessions.get(&session_id) {
                let _ = handle.send(SessionMessage::UpdateActiveTools {
                    tool_definitions: rebuilt,
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

        // Clean up per-session workspace mappings
        self.session_workspaces.remove(session_id);
        self.pending_workspaces.remove(session_id);

        tracing::info!(session_id = %session_id, "SessionManager: destroyed session");
        Ok(())
    }

    /// Send a message to a specific session.
    ///
    /// Returns an error if the session does not exist or the channel is closed.
    /// When the channel is closed (e.g. the SessionTask panicked or was evicted
    /// without cleanup), the dead handle is auto-removed so subsequent calls
    /// get a clean "Session not found" instead of "channel closed".
    pub fn send_to_session(
        &mut self,
        session_id: &str,
        msg: SessionMessage,
    ) -> Result<()> {
        let handle = self.sessions.get(session_id).ok_or_else(|| {
            RuntimeError::Config(format!("Session not found: {}", session_id))
        })?;

        if let Err(_send_err) = handle.send(msg) {
            // Channel closed — the SessionTask has died (panic / eviction race).
            // Auto-remove the stale handle so the next attempt gets a clean
            // "Session not found" error instead of "channel closed".
            let was_finished = handle.join_handle.is_finished();
            self.sessions.remove(session_id);
            tracing::warn!(
                session_id = %session_id,
                task_finished = was_finished,
                "Session channel closed — auto-removing dead session handle"
            );
            Err(RuntimeError::Config(format!(
                "Session not found: {}",
                session_id
            )))
        } else {
            Ok(())
        }
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
        shell_approval_threshold: Option<String>,
    ) -> Vec<String> {
        self.runtime_overrides.merge(
            max_output_tokens,
            max_iterations,
            temperature,
            system_prompt_override.clone(),
            None, // active_tools handled separately via apply_active_tools
            shell_approval_threshold.clone(),
        );
        // ── 1. Broadcast to SessionTask channels (for tool definitions etc.) ──
        let sessions = self.broadcast(SessionMessage::UpdateRuntimeConfig {
            max_output_tokens,
            max_iterations,
            temperature,
            system_prompt_override: system_prompt_override.clone(),
            shell_approval_threshold: shell_approval_threshold.clone(),
        });

        // ── 2. Also deliver via send_inbound() fast channel ──
        // This ensures the AgentLoop immediately picks up runtime config
        // changes even while mid-execution (streaming / running tools),
        // when the SessionTask's message loop is blocked on agent_loop.run().
        let user_op = UserOp::UpdateRuntimeConfig {
            max_output_tokens,
            max_iterations,
            temperature,
            system_prompt_override,
            shell_approval_threshold,
        };
        let inbound_msg = InboundMessage::UserOperation(user_op);
        for (session_id, handle) in &self.sessions {
            if let Err(e) = handle.send_inbound(inbound_msg.clone()) {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to deliver UpdateRuntimeConfig via send_inbound (session channel may be full or closed)"
                );
            }
        }

        sessions
    }

    /// Apply active tools override from Gateway RuntimeConfigUpdate.
    ///
    /// This rebuilds `tool_definitions` from the full tool specs registry
    /// (stored in `SessionManagerConfig.full_tool_specs`) filtered by the
    /// given `active_tools` list, then broadcasts the new definitions to
    /// all active sessions. The override is also cached so sessions created
    /// *after* this call inherit the correct tool set.
    ///
    /// When `active_tools` is `None`, the override is cleared and
    /// `tool_definitions` is NOT rebuilt (caller should send a separate
    /// update with the full list if needed).
    ///
    /// Returns the list of session IDs that failed to receive the update.
    pub fn apply_active_tools(
        &mut self,
        active_tools: Option<Vec<String>>,
    ) -> Vec<String> {
        // Cache the override when Some; None preserves existing value.
        if active_tools.is_some() {
            self.runtime_overrides.active_tools = active_tools.clone();
        }

        // Build the new tool definitions
        let tool_definitions = match active_tools.as_ref() {
            Some(names) => crate::agent::context::build_tool_definitions_from_names(
                names,
                &self.config.full_tool_specs,
            ),
            // None = "keep current" — don't rebuild, just broadcast current
            None => return Vec::new(),
        };

        tracing::info!(
            tool_count = tool_definitions.len(),
            active_tool_names = ?active_tools,
            "SessionManager: applying active tools override"
        );

        // Update the config so new sessions inherit the rebuilt definitions
        self.config.tool_definitions = tool_definitions.clone();

        // Broadcast to all active sessions
        self.broadcast(SessionMessage::UpdateActiveTools {
            tool_definitions,
        })
    }

    /// Apply MCP server configuration changes from Gateway RuntimeConfigUpdate.
    ///
    /// Connects to (or disconnects from) MCP servers and updates:
    ///   - `self.mcp_tools` — the tool wrappers for dispatch
    ///   - `self.config.full_tool_specs` — LLM-facing tool definitions
    ///   - `self.config.tool_definitions` — current active tool definitions
    ///
    /// When `configs` is `Some(vec![])`, all MCP servers are disconnected.
    /// When `configs` is `Some(non_empty)`, MCP servers are (re)connected.
    pub async fn apply_mcp_servers(
        &mut self,
        configs: Vec<rollball_core::protocol::McpServerConfigDef>,
    ) {
        use rollball_core::tools::traits::Tool;

        if configs.is_empty() {
            tracing::info!("SessionManager: disconnecting all MCP servers");
            // Disconnect existing MCP connections to release resources
            self.mcp_manager.disconnect().await;
            self.mcp_tools = None;
            // Notify all sessions that MCP tools are gone
            self.broadcast(SessionMessage::UpdateMcpTools { mcp_tools: None });
            // Rebuild full_tool_specs without MCP tools
            self.rebuild_full_tool_specs_with_mcp();
            // Rebuild tool_definitions from the updated specs.
            // When active_tools is None → use all available tools.
            let active_tools_ref = self.runtime_overrides.active_tools.as_deref().unwrap_or(&[]);
            let rebuilt = crate::agent::context::build_tool_definitions_from_names(
                active_tools_ref,
                &self.config.full_tool_specs,
            );
            self.config.tool_definitions = rebuilt.clone();
            self.broadcast(SessionMessage::UpdateActiveTools {
                tool_definitions: rebuilt,
            });
            return;
        }

        // Disconnect previous MCP connections before connecting new ones
        self.mcp_manager.disconnect().await;

        let (registry, wrappers, _specs) = self.mcp_manager.connect(&configs).await;

        // Store MCP tool wrappers (Arc<dyn Tool>) for dispatch
        let mcp_tool_arcs: Vec<Arc<dyn Tool>> = wrappers
            .into_iter()
            .map(|w| Arc::new(w) as Arc<dyn Tool>)
            .collect();
        self.mcp_tools = Some(mcp_tool_arcs.clone());

        // Push MCP tools to all existing sessions so AgentCore.all_tools
        // is updated for both LLM dispatch and debug snapshot capture.
        self.broadcast(SessionMessage::UpdateMcpTools {
            mcp_tools: Some(mcp_tool_arcs),
        });

        // Update full_tool_specs to include MCP tool specs
        self.rebuild_full_tool_specs_with_mcp();

        // Update tool_definitions to include MCP tools
        if let Some(ref active_tools) = self.runtime_overrides.active_tools {
            let rebuilt = crate::agent::context::build_tool_definitions_from_names(
                active_tools,
                &self.config.full_tool_specs,
            );
            self.config.tool_definitions = rebuilt.clone();
            self.broadcast(SessionMessage::UpdateActiveTools {
                tool_definitions: rebuilt,
            });
        }

        tracing::info!(
            server_count = registry.server_count(),
            tool_count = registry.tool_count(),
            "SessionManager: MCP servers applied"
        );
    }

    /// Rebuild `full_tool_specs` by merging the original built-in specs with
    /// any currently connected MCP tool specs.
    fn rebuild_full_tool_specs_with_mcp(&mut self) {
        // Start from the original built-in tool specs (stored at init time).
        // We store these separately to avoid losing them on rebuild.
        let mut specs = self.config.full_tool_specs.clone();

        // Remove any previous MCP entries (prefixed with "mcp:")
        specs.retain(|(name, _)| !name.starts_with("mcp:"));

        // Add current MCP tool specs
        if let Some(ref wrappers) = self.mcp_tools {
            for tool in wrappers {
                let tool_spec = tool.spec();
                let serialized = serde_json::to_value(&tool_spec).unwrap_or_default();
                specs.push((tool_spec.name, serialized));
            }
        }

        self.config.full_tool_specs = specs;
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

    /// Update web search config from Gateway SearchConfigDelivery hot-push.
    ///
    /// Caches the search key vault and provider list (mirrors CachedLLMConfig pattern)
    /// so that ConfigSnapshot can return current search provider metadata.
    /// Search keys are NEVER persisted to disk — only held in memory.
    pub fn update_search_config(
        &mut self,
        search_key_vault: Vec<SearchKeyEntry>,
        search_list: Vec<SearchProviderListItem>,
    ) {
        tracing::info!(
            provider_count = search_list.len(),
            key_count = search_key_vault.len(),
            "SessionManager: caching search config"
        );
        self.cached_search_config = Some(CachedSearchConfig {
            key_vault: search_key_vault,
            provider_list: search_list,
        });
    }

    /// Update user identity from Gateway UserProfileUpdate push.
    ///
    /// Formats the `UserProfile` into an `identity_context` text block
    /// and broadcasts it to all active sessions via their ContextBuilder.
    pub fn update_user_identity(
        &mut self,
        profile: Option<rollball_core::protocol::UserProfile>,
    ) {
        let identity_context = profile.as_ref().map(|p| format_user_profile_context(p));
        tracing::info!(
            has_profile = profile.is_some(),
            ctx_len = identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
            "SessionManager: updating user identity"
        );
        self.config.identity_context = identity_context.clone();
        // Broadcast updated identity to all active sessions
        for (_sid, handle) in &self.sessions {
            let _ = handle.send(SessionMessage::UpdateIdentityContext {
                identity_context: identity_context.clone(),
            });
        }
    }

    /// Get the current cached search config, if any.
    pub(crate) fn search_config(&self) -> Option<&CachedSearchConfig> {
        self.cached_search_config.as_ref()
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

    /// Get the current status of all active sessions (ADR-014).
    ///
    /// Returns a map from session_id → SessionStatus for sessions currently
    /// running in memory. Sessions that exist only on disk (scanned by
    /// `list_sessions`) won't appear here.
    pub fn session_statuses(&self) -> Vec<(String, SessionStatus)> {
        self.sessions
            .iter()
            .map(|(id, handle)| (id.clone(), handle.status()))
            .collect()
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

    /// Get the current session ID (used as the default routing target).
    pub fn current_session_id(&self) -> &str {
        &self.current_session_id
    }

    /// Set the current session ID (called when the user activates a session).
    pub fn set_current_session_id(&mut self, session_id: String) {
        tracing::info!(
            old_session_id = %self.current_session_id,
            new_session_id = %session_id,
            "SessionManager: current session updated"
        );
        self.current_session_id = session_id;
    }

    /// Resolve the target session ID for an incoming message.
    ///
    /// If `explicit_id` is Some and non-empty, use it; otherwise fall back
    /// to the current session ID. This replaces the scattered logic in
    /// cli.rs that was doing the same thing inline.
    ///
    /// Returns `None` when both explicit_id and current_session_id are
    /// absent/empty (e.g. before any session has been created).
    pub fn resolve_target_session(&self, explicit_id: Option<&str>) -> Option<String> {
        explicit_id
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                if self.current_session_id.is_empty() {
                    tracing::warn!(
                        "resolve_target_session: no explicit session_id and current_session_id is empty — no session created yet?"
                    );
                    None
                } else {
                    Some(self.current_session_id.clone())
                }
            })
    }

    /// Evict idle sessions from memory.
    ///
    /// A session is evicted when ALL of the following conditions are met:
    /// 1. Its status is `Idle` (not Streaming/WaitingApproval/Paused)
    /// 2. It has been idle for longer than `idle_timeout`
    /// 3. It is NOT the current active session
    ///
    /// Eviction destroys the in-memory SessionTask but leaves the JSONL
    /// file on disk. The session can be re-activated later via lazy resume
    /// in the `activate_session` handler.
    pub async fn evict_idle_sessions(&mut self, idle_timeout: std::time::Duration) {
        let current = self.current_session_id.clone();
        let mut to_evict = Vec::new();

        for (session_id, handle) in &self.sessions {
            if *session_id == current {
                continue;
            }
            if handle.status() != SessionStatus::Idle {
                continue;
            }
            let elapsed = handle.last_active_at().elapsed();
            if elapsed >= idle_timeout {
                to_evict.push(session_id.clone());
            }
        }

        if to_evict.is_empty() {
            return;
        }

        for session_id in &to_evict {
            if let Some(handle) = self.sessions.remove(session_id) {
                let _ = handle.inbound_tx.send(SessionMessage::Stop).await;
                tracing::info!(session_id = %session_id, "Evicted idle session from memory (idle > {:?})", idle_timeout);
            }
        }
        tracing::info!(evicted = to_evict.len(), "Idle session eviction complete");
    }

    // ── per-session workspace management ─────────────────────────────────

    /// Get the agent home path (derived from core config).
    pub fn agent_home(&self) -> &str {
        &self.core.config().work_dir
    }

    /// Set the current workspace for a specific session.
    ///
    /// Updates both the in-memory map and persists to the session's JSONL
    /// conversation file (when one exists).
    pub fn set_session_workspace(&mut self, session_id: &str, workspace_id: &str) {
        self.session_workspaces
            .insert(session_id.to_string(), workspace_id.to_string());
        // Remove from pending if the workspace is now active
        self.pending_workspaces.remove(session_id);
        tracing::info!(
            session_id = %session_id,
            workspace_id = %workspace_id,
            "SessionManager: session workspace updated"
        );
        // Persist to the session's JSONL conversation file
        if let Some(handle) = self.sessions.get(session_id) {
            let _ = handle.send(SessionMessage::SetWorkspaceId {
                workspace_id: workspace_id.to_string(),
            });
        }
    }

    /// Get the current workspace ID for a session.
    /// Returns `"__agent_home__"` if the session has no explicit workspace set.
    pub fn session_workspace_id(&self, session_id: &str) -> &str {
        self.session_workspaces
            .get(session_id)
            .map(|s| s.as_str())
            .unwrap_or("__agent_home__")
    }

    /// Get the current working directory path for a session.
    /// Returns `(path, is_agent_home)`.
    pub fn current_dir_for(&self, session_id: &str, resolver: &WorkspaceResolver) -> (String, bool) {
        let ws_id = self.session_workspace_id(session_id);
        if ws_id == "__agent_home__" {
            return (resolver.agent_home().to_string(), true);
        }
        match resolver.find_by_id(ws_id) {
            Some(dir) => (dir.path.clone(), false),
            None => {
                tracing::warn!(
                    session_id = %session_id,
                    workspace_id = %ws_id,
                    "Session workspace not found in resolver, falling back to agent home"
                );
                (resolver.agent_home().to_string(), true)
            }
        }
    }

    /// Format and send workspace context to a specific session only.
    pub fn update_session_workspace_context(
        &mut self,
        session_id: &str,
        resolver: &WorkspaceResolver,
    ) {
        let ws_id = self.session_workspace_id(session_id);
        let context_text = format_workspace_context_for_session(resolver, ws_id);
        if let Some(handle) = self.sessions.get(session_id) {
            let _ = handle.send(SessionMessage::UpdateWorkspaceContext {
                context_text,
            });
            tracing::info!(
                session_id = %session_id,
                workspace_id = %ws_id,
                "SessionManager: sent per-session workspace context"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                "SessionManager: cannot update workspace context — session not found"
            );
        }
    }

    /// Set the default workspace ID for new sessions.
    /// When set to a workspace ID other than "__agent_home__", newly created
    /// sessions will use this workspace instead of agent home.
    pub fn set_default_workspace_id(&mut self, workspace_id: &str) {
        self.default_workspace_id = workspace_id.to_string();
        tracing::info!(
            default_workspace_id = %workspace_id,
            "SessionManager: default workspace updated for new sessions"
        );
    }

    /// Reconcile deleted workspaces: for all sessions whose selected workspace
    /// is no longer in the resolver's allowed list, move to pending and fallback
    /// to agent home.
    pub fn reconcile_deleted_workspaces(&mut self, resolver: &WorkspaceResolver) {
        let mut changes: Vec<(String, String)> = Vec::new();
        for (sid, ws_id) in &self.session_workspaces {
            if ws_id == "__agent_home__" {
                continue;
            }
            if resolver.find_by_id(ws_id).is_none() {
                changes.push((sid.clone(), ws_id.clone()));
            }
        }
        for (sid, old_ws_id) in changes {
            self.pending_workspaces.insert(sid.clone(), old_ws_id.clone());
            self.session_workspaces.insert(sid.clone(), "__agent_home__".to_string());
            tracing::info!(
                session_id = %sid,
                old_workspace_id = %old_ws_id,
                "SessionManager: workspace deleted, moved to pending + fallback to agent home"
            );
        }
    }

    /// Get the pending workspace ID for a session, if any.
    pub fn pending_workspace_id(&self, session_id: &str) -> Option<&str> {
        self.pending_workspaces.get(session_id).map(|s| s.as_str())
    }
}

/// Format a `UserProfile` into an identity context text block for the LLM system prompt.
///
/// Produces a human-readable summary like:
///   - Display Name: Alice
///   - Language: zh-CN
///   - Timezone: Asia/Shanghai
///   - City: Shanghai
///   - Country: CN
///   - Occupation: Software Engineer
///   - Communication Style: concise
pub(crate) fn format_user_profile_context(profile: &rollball_core::protocol::UserProfile) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("- Display Name: {}", profile.display_name));
    lines.push(format!("- Language: {}", profile.language));
    lines.push(format!("- Timezone: {}", profile.timezone));
    if let Some(ref city) = profile.city {
        lines.push(format!("- City: {}", city));
    }
    if let Some(ref country) = profile.country {
        lines.push(format!("- Country: {}", country));
    }
    if let Some(ref occupation) = profile.occupation {
        lines.push(format!("- Occupation: {}", occupation));
    }
    if let Some(ref style) = profile.communication_style {
        lines.push(format!("- Communication Style: {}", style));
    }
    for (key, value) in &profile.custom {
        lines.push(format!("- {}: {}", key, value));
    }
    lines.join("\n")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use rollball_core::providers::mock::MockProvider;

    fn make_tool_spec(name: &str) -> (String, serde_json::Value) {
        let schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("Tool {}", name),
                "parameters": { "type": "object", "properties": {} }
            }
        });
        (name.to_string(), schema)
    }

    // ── RuntimeConfigOverrides ─────────────────────────────────────────

    #[test]
    fn test_overrides_is_empty() {
        let ov = RuntimeConfigOverrides::default();
        assert!(ov.is_empty());
    }

    #[test]
    fn test_overrides_merge_active_tools() {
        let mut ov = RuntimeConfigOverrides::default();
        ov.merge(None, None, None, None, Some(vec!["tool_a".into()]), None);
        assert!(!ov.is_empty());
        assert_eq!(ov.active_tools.as_deref(), Some(&["tool_a".to_string()][..]));

        // Re-merge with Some replaces
        ov.merge(None, None, None, None, Some(vec!["tool_b".into()]), None);
        assert_eq!(ov.active_tools.as_deref(), Some(&["tool_b".to_string()][..]));

        // None preserves
        ov.merge(None, None, None, None, None, None);
        assert_eq!(ov.active_tools.as_deref(), Some(&["tool_b".to_string()][..]));
    }

    #[test]
    fn test_overrides_empty_vec_clears_tools() {
        let mut ov = RuntimeConfigOverrides::default();
        ov.merge(None, None, None, None, Some(vec!["tool_a".into()]), None);
        ov.merge(None, None, None, None, Some(vec![]), None);
        assert_eq!(ov.active_tools, Some(vec![]));
    }

    // ── apply_active_tools ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_apply_active_tools_with_sessions() {
        let manifest = rollball_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.tools"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "mock"
            model = "mock-model"
            "#
        ).unwrap();

        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let core = Arc::new(AgentCore::new(config, manifest, provider, vec![], None));

        let mut mgr_config = SessionManagerConfig::default();
        mgr_config.full_tool_specs = vec![make_tool_spec("tool_a"), make_tool_spec("tool_b")];

        let mut mgr = SessionManager::new(core, mgr_config, String::new());

        // Apply active_tools
        let failed = mgr.apply_active_tools(Some(vec!["tool_a".to_string()]));
        assert!(failed.is_empty());
        assert_eq!(mgr.config.tool_definitions.len(), 1);
        assert_eq!(mgr.runtime_overrides.active_tools, Some(vec!["tool_a".to_string()]));
    }

    #[tokio::test]
    async fn test_apply_active_tools_none_noop() {
        let manifest = rollball_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.tools"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "mock"
            model = "mock-model"
            "#
        ).unwrap();

        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let core = Arc::new(AgentCore::new(config, manifest, provider, vec![], None));

        let mgr_config = SessionManagerConfig::default();
        let mut mgr = SessionManager::new(core, mgr_config, String::new());

        // apply_active_tools(None) should return empty and not crash
        let failed = mgr.apply_active_tools(None);
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn test_apply_active_tools_broadcasts_to_sessions() {
        let manifest = rollball_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.broadcast"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "mock"
            model = "mock-model"
            "#
        ).unwrap();

        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let core = Arc::new(AgentCore::new(config, manifest, provider, vec![], None));

        let mut mgr_config = SessionManagerConfig::default();
        mgr_config.full_tool_specs = vec![make_tool_spec("tool_x")];
        let mut mgr = SessionManager::new(core, mgr_config, String::new());

        // Create a session first
        let sid = mgr.create_session_with_id("s1".to_string()).await.unwrap();
        assert_eq!(sid, "s1");

        // Apply active_tools — should broadcast to s1
        let failed = mgr.apply_active_tools(Some(vec!["tool_x".to_string()]));
        assert!(failed.is_empty());
    }

    // ── resolve_target_session ─────────────────────────────────────────

    fn make_manager_with_current_id(current_id: &str) -> SessionManager {
        let manifest = rollball_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.resolve"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "mock"
            model = "mock-model"
            "#
        ).unwrap();
        let config = RuntimeConfig::default();
        let provider = Arc::new(MockProvider::single_text("OK"));
        let core = Arc::new(AgentCore::new(config, manifest, provider, vec![], None));
        SessionManager::new(core, SessionManagerConfig::default(), current_id.to_string())
    }

    #[test]
    fn test_resolve_target_session_explicit_id_wins() {
        let mgr = make_manager_with_current_id("current-sid");
        assert_eq!(mgr.resolve_target_session(Some("explicit-sid")), Some("explicit-sid".to_string()));
    }

    #[test]
    fn test_resolve_target_session_empty_explicit_falls_back() {
        let mgr = make_manager_with_current_id("current-sid");
        assert_eq!(mgr.resolve_target_session(Some("")), Some("current-sid".to_string()));
    }

    #[test]
    fn test_resolve_target_session_none_falls_back() {
        let mgr = make_manager_with_current_id("current-sid");
        assert_eq!(mgr.resolve_target_session(None), Some("current-sid".to_string()));
    }

    #[test]
    fn test_resolve_target_session_both_empty_returns_none() {
        let mgr = make_manager_with_current_id("");
        assert_eq!(mgr.resolve_target_session(None), None);
    }

    #[test]
    fn test_resolve_target_session_empty_explicit_and_empty_current_returns_none() {
        let mgr = make_manager_with_current_id("");
        assert_eq!(mgr.resolve_target_session(Some("")), None);
    }
}
