//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::Budget;
use acowork_core::protocol::ProtocolType;
use acowork_core::protocol::{SearchKeyEntry, SearchProviderListItem};
use acowork_core::tools::traits::Tool;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::loop_::SessionChunkEvent;
use crate::agent::session::session_handle::SessionHandle;
use crate::agent::session::session_task::{SessionMessage, SessionTask};
use crate::agent::session_state::{SessionState, SessionStatus};
use crate::config::DEFAULT_TEMPERATURE;
use crate::conversation::ConversationSession;
use crate::debug::controller::DebugController;
use crate::error::{Result, RuntimeError};
use crate::tools::mcp_manager::McpConnectionFailure;
use crate::tools::mcp_manager::McpManager;
use crate::tools::workspace_resolver::{WorkspaceResolver, format_workspace_context_for_session};
use acowork_mcp::client::McpRegistry;
use acowork_mcp::wrapper::McpToolWrapper;

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
    /// Optional streaming chunk sender shared across all sessions.
    /// When set, each session's AgentLoop forwards ChunkEvents here
    /// so the caller can relay them to Gateway.
    pub chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Dedicated control-event channel (Stopped, SessionStateChanged, Done,
    /// Error, etc.). Separated from `chunk_tx` to prevent data-event
    /// backpressure from blocking control events.
    pub control_chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Complete tool definitions (with input_schema) for ContextBuilder.
    /// SessionTask uses these instead of building simplified ones from manifest.
    pub tool_definitions: Vec<serde_json::Value>,
    /// Full tool specs (name, schema) for ALL registered built-in tools.
    /// Stored so that tool definitions can be hot-rebuilt when `active_tools`
    /// changes without requiring access to the ToolRegistry (which is behind Arc).
    pub full_tool_specs: Vec<(String, serde_json::Value)>,
    /// Identity context string injected by Gateway for ContextBuilder.
    pub identity_context: Option<String>,
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
            chunk_tx: None,
            control_chunk_tx: None,
            tool_definitions: Vec::new(),
            full_tool_specs: Vec::new(),
            identity_context: None,
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
    pub shell_approval_threshold: Option<String>,
}

impl RuntimeConfigOverrides {
    /// Returns true when no override value has been set.
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.temperature.is_none()
            && self.system_prompt_override.is_none()
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
        if shell_approval_threshold.is_some() {
            self.shell_approval_threshold = shell_approval_threshold;
        }
    }
}

/// Pending embedding config from Gateway EmbeddingConfigUpdate.
///
/// Stored so that the config can be persisted to `agent_config.json`
/// and used on next Agent restart to rebuild the FallbackEmbeddingProvider.
/// True hot-swap (in-place rebuild without restart) is planned future work.
#[derive(Debug, Clone)]
pub struct PendingEmbedConfig {
    pub embed_endpoint: String,
    pub embed_model_id: String,
    pub embed_dimension: usize,
}

/// Debug mode handles injected at runtime when Gateway pushes
/// EnableDebugMode. Stored on SessionManager so that sessions
/// created *after* debug mode is enabled inherit the debug
/// controller, event sender, and notify handles.
///
/// Re-exported from `crate::debug::DebugHandles` for convenience.
use crate::debug::DebugHandles;

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
    /// Shared WorkspaceResolver for resolving workspace_id → filesystem path.
    /// Set once via `set_resolver()` after construction. When available,
    /// `set_session_workspace()` will also send `SetWorkDir` to the session
    /// so that `AgentCore::current_work_dir` is kept in sync automatically.
    resolver: Option<Arc<std::sync::RwLock<WorkspaceResolver>>>,
    /// Runtime-injected debug handles (set when Gateway pushes EnableDebugMode).
    /// When Some, new sessions inherit the debug controller, event sender,
    /// and notify handles. Existing sessions restart via urgent_interrupt
    /// and pick up these handles on their next agent_loop.run().
    pub(crate) runtime_debug_handles: Option<DebugHandles>,
    /// Per-session debug controllers, shared with DebugProtocolServer for
    /// request routing. Each session adds its controller when created with
    /// debug mode active.
    pub(crate) debug_controllers:
        Arc<tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>>,
    /// Per-session urgent_stop Notify handles.
    /// Keyed by session_id; fire_urgent_stop() looks up the target session's
    /// Notify and wakes only that session's tokio::select! branches.
    urgent_stops: HashMap<String, Arc<Notify>>,
    /// Pending embedding config from Gateway EmbeddingConfigUpdate.
    /// Stored for persistence and used on next Agent restart.
    pub pending_embed_config: Option<PendingEmbedConfig>,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        Self {
            core,
            sessions: HashMap::new(),
            config,
            runtime_overrides: RuntimeConfigOverrides::default(),
            mcp_tools: None,
            mcp_manager: McpManager::new(),
            session_workspaces: HashMap::new(),
            pending_workspaces: HashMap::new(),
            default_workspace_id: "__agent_home__".to_string(),
            resolver: None,
            runtime_debug_handles: None,
            debug_controllers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            urgent_stops: HashMap::new(),
            pending_embed_config: None,
        }
    }

    /// Set the shared WorkspaceResolver.
    ///
    /// Must be called once after construction (before any session is created)
    /// so that `set_session_workspace()` can resolve workspace IDs to actual
    /// filesystem paths and send `SetWorkDir` to sessions.
    pub fn set_resolver(&mut self, resolver: Arc<std::sync::RwLock<WorkspaceResolver>>) {
        self.resolver = Some(resolver);
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
        self.create_session_with_id_and_conversation(session_id, None)
            .await
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
        // Read the persisted workspace_id and model/provider before the conversation
        // is moved into SessionState, so we can restore them.
        let persisted_workspace_id = conversation
            .as_ref()
            .and_then(|c| c.workspace_id())
            .map(|w| w.to_string());

        let (inbound_tx, inbound_rx) = mpsc::channel(self.config.inbound_channel_capacity);

        let session_state = self.build_initial_session_state(conversation);

        // Shared channel for bypass-injecting debug handles into AgentCore
        // while the agent loop is running (its message channel is blocked).
        let pending_debug_handles: Arc<tokio::sync::Mutex<Option<DebugHandles>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        // If debug mode is active, create a per-session DebugController and
        // register it in self.debug_controllers so the DebugProtocolServer can
        // read this session's state via getState. The global runtime_debug_handles
        // carries a shared controller — we must NOT reuse it because each session
        // needs its own independent iteration/phase.
        // The notify handles (rewind/resume) also come from the per-session
        // controller so the debug server's notify_one() calls align with SessionTask.
        let per_session_debug = if let Some(ref handles) = self.runtime_debug_handles {
            let ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
            let (per_rewind, per_resume) = {
                let guard = ctrl.lock().await;
                (guard.rewind_notify_handle(), guard.resume_notify_handle())
            };
            self.debug_controllers
                .write()
                .await
                .insert(session_id.clone(), ctrl.clone());
            Some(DebugHandles {
                debug_ctrl: ctrl,
                debug_event_tx: handles.debug_event_tx.for_session(session_id.clone()),
                rewind_notify: per_rewind,
                resume_notify: per_resume,
            })
        } else {
            None
        };

        // Resolve initial workspace directory for direct initialization.
        // This avoids relying on the SetWorkDir message replay to set
        // AgentCore.current_work_dir — it's applied during session creation.
        let initial_workspace = persisted_workspace_id
            .clone()
            .unwrap_or_else(|| self.default_workspace_id.clone());
        // Pre-register the workspace mapping so current_dir_for works.
        self.session_workspaces
            .insert(session_id.clone(), initial_workspace.clone());
        self.pending_workspaces.remove(&session_id);
        let initial_work_dir = if let Some(ref resolver) = self.resolver {
            let guard = resolver.read().unwrap();
            if initial_workspace == "__agent_home__" {
                guard.agent_home().to_string()
            } else {
                guard
                    .find_by_id(&initial_workspace)
                    .map(|d| d.path.clone())
                    .unwrap_or_else(|| guard.agent_home().to_string())
            }
        } else {
            self.core.config.work_dir.clone()
        };

        let (mut task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            self.config.control_chunk_tx.clone(),
            session_id.clone(),
            self.config.tool_definitions.clone(),
            self.config.identity_context.clone(),
            self.config.protocol_type.clone(),
            self.mcp_tools.clone(),
            per_session_debug,
            pending_debug_handles.clone(),
            self.runtime_overrides.clone(),
            Some(initial_work_dir),
        );

        // ADR-014: Create watch channel for session status
        let (status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        task.set_status_tx(status_tx);

        // Create shared snapshot slot for the Gateway session state pull API.
        // The slot is written by AgentLoop::emit_session_state and read by
        // snapshot_session_state() without any message passing.
        let snapshot_slot: Arc<std::sync::RwLock<Option<crate::agent::session_state::SessionStateSnapshot>>> =
            Arc::new(std::sync::RwLock::new(None));
        task.set_snapshot_slot(snapshot_slot.clone());

        // Register per-session urgent_stop Notify so fire_urgent_stop()
        // only wakes this session's tokio::select! branches.
        if let Some(notify) = task.urgent_stop_notify() {
            self.urgent_stops.insert(session_id.clone(), notify);
        }

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
            pending_debug_handles: pending_debug_handles.clone(),
            snapshot_slot,
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        // Initialize per-session workspace.
        // For resumed sessions, restore the persisted workspace_id from JSONL metadata.
        // New sessions default to last_active workspace (or agent home fallback).
        // Note: the workspace mapping was already pre-registered above for
        // initial_work_dir resolution. This call persists workspace_id to JSONL
        // and sends SetWorkDir (redundant with direct init, but harmless).
        self.set_session_workspace(&session_id, &initial_workspace);

        // Apply workspace context + prompt file from the resolver.
        //
        // This is the single source of truth for per-session workspace state
        // injection. `set_resolver()` is a hard precondition for session
        // creation (see its doc); `update_session_workspace_context` will
        // panic via `.expect()` if it was not called (programming error).
        // By injecting at creation time, every session path — initial
        // session, "New Chat", lazy resume — bootstraps identically with no
        // caller-side follow-up required.
        self.update_session_workspace_context(&session_id);

        // Provider list / capabilities / max-output limits are now read
        // on demand from the shared `AgentCore.global_provider_list`
        // populated at AgentHello and updated via ProviderListUpdate. No
        // per-session replay is required — sessions query AgentCore directly.

        Ok(session_id)
    }

    /// Build a fully-initialized SessionState for a new or resumed session.
    /// All per-session fields are set synchronously before this returns.
    /// Caller must hold an Arc<AgentCore> with global_provider_list populated.
    fn build_initial_session_state(
        &self,
        conversation: Option<ConversationSession>,
    ) -> SessionState {
        let mut initial_model = conversation.as_ref().and_then(|c| c.model());
        let mut initial_provider = conversation.as_ref().and_then(|c| c.provider());

        // Fall back to Runtime-internal default when the session has no
        // explicit model/provider (new agent, first session ever created).
        // current_model_and_provider() atomically returns the (model, provider)
        // pair from the most recently active session, or the first entry from
        // global_provider_list if no session has ever been activated.
        if initial_model.is_none() || initial_provider.is_none() {
            let (fallback_model, fallback_provider) = self.current_model_and_provider();

            // Persist the fallback to JSONL so activate_session returns
            // consistent metadata to the frontend on subsequent requests.
            if let (Some(conv), Some(model)) = (&conversation, &fallback_model)
                && conv.model().is_none()
            {
                conv.update_model_provider(model, fallback_provider.as_deref());
            }
            initial_model = fallback_model;
            initial_provider = fallback_provider;
        }

        // Resume path: rebuild HistoryManager from the JSONL log so the LLM
        // sees the prior conversation on the first new turn after cold-start.
        // This is gated by ACOWORK_DISABLE_SESSION_RESUME=1 for ops debugging.
        let restored = if std::env::var("ACOWORK_DISABLE_SESSION_RESUME")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::warn!(
                "ACOWORK_DISABLE_SESSION_RESUME set; skipping JSONL history restore"
            );
            None
        } else {
            conversation.as_ref().and_then(|conv| {
                let path = conv.session_path();
                match crate::agent::session::restorer::restore_history_from_jsonl(path) {
                    Ok(outcome) if !outcome.messages.is_empty() => {
                        tracing::info!(
                            session_id = %conv.session_id(),
                            replayed = outcome.replayed_entry_count,
                            skipped = outcome.skipped_entry_count,
                            had_compaction = outcome.had_compaction,
                            messages = outcome.messages.len(),
                            "Session resume: restored history from JSONL"
                        );
                        Some(outcome)
                    }
                    Ok(_) => {
                        // New session or empty file — nothing to restore.
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %conv.session_id(),
                            error = %e,
                            "Session resume: failed to restore history; starting empty"
                        );
                        None
                    }
                }
            })
        };

        let mut session_state = SessionState::new(
            self.config.history_max_tokens,
            self.config.per_session_budget.clone(),
            conversation,
        );

        // ADR-012: Set per-session model/provider on SessionState (only if we have one).
        if let Some(m) = initial_model.as_ref() {
            session_state.set_model(m.clone());
            // Update HistoryManager::max_tokens to the model's actual effective
            // input budget rather than the static config.history_max_tokens (128K).
            // Without this, trim_fifo would clamp history at 128K which may be
            // far below the model's actual context window, making auto compaction
            // at 80% threshold unreachable.
            let budget = self.core.context_trim_budget(m);
            session_state.history_mut().set_max_tokens(budget);

            // Three-level priority chain for reasoning_effort:
            // 1. Persisted session value (from JSONL metadata, via ConversationSession)
            // 2. Provider capabilities default_reasoning_effort
            // 3. None (provider does not support thinking control)
            let persisted_effort = session_state
                .conversation()
                .and_then(|c| c.reasoning_effort());

            if let Some(ref effort_str) = persisted_effort {
                // Session already has a persisted value; restore it.
                let effort = acowork_core::providers::traits::ReasoningEffort::from_str_loose(effort_str);
                session_state.set_reasoning_effort(effort);
            } else {
                // No persisted value: initialize from provider capabilities default.
                // If default_reasoning_effort is None but supports_reasoning is true,
                // fall back to Auto so the user sees the reasoning effort control.
                let caps = self.core.get_model_capabilities(m);
                let provider_default = caps
                    .as_ref()
                    .and_then(|c| c.default_reasoning_effort.clone());
                let effort = provider_default
                    .as_deref()
                    .and_then(acowork_core::providers::traits::ReasoningEffort::from_str_loose)
                    .or_else(|| {
                        // Model supports reasoning but has no explicit default → Auto
                        if caps.as_ref().and_then(|c| c.supports_reasoning).unwrap_or(false) {
                            Some(acowork_core::providers::traits::ReasoningEffort::Auto)
                        } else {
                            None
                        }
                    });
                session_state.set_reasoning_effort(effort.clone());
                // Write back to ConversationSession so future resumes have a value.
                if let Some(conv) = session_state.conversation() {
                    let effort_str = effort.as_ref().map(|e| e.to_string());
                    conv.update_reasoning_effort(effort_str);
                }
            }
        }
        if let Some(p) = initial_provider.as_ref() {
            session_state.set_provider(p.clone());
        }

        // Propagate temperature override to the session with fallback chain:
        // runtime_overrides → core.temperature_override → DEFAULT_TEMPERATURE.
        // Always set a concrete value so the model actually receives the configured
        // temperature and the status panel can display it accurately.
        let temperature = self
            .runtime_overrides
            .temperature
            .or(self.core.temperature_override)
            .or(Some(DEFAULT_TEMPERATURE));
        session_state.set_temperature(temperature);

        // Install the restored history *after* set_max_tokens has been applied,
        // so the lossless trim (if needed) operates against the model-correct
        // budget. Trim is the safety net for the "resumed under a smaller
        // model" case — it never invokes an LLM.
        if let Some(outcome) = restored {
            session_state.history_mut().load_restored(outcome.messages);
            let dropped = session_state.history_mut().fit_to_budget_lossless();
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    "Session resume: history exceeded 80% budget under current model; \
                     applied lossless tail-preserving trim"
                );
            }
            // If a compaction summary was restored, the session is logically
            // already in a "post-compaction" state — mark it so session-close
            // tail distillation respects the boundary.
            if outcome.had_compaction {
                session_state.is_compacted = true;
            }
        }

        session_state
    }

    /// Close a session by ID, sending a Close message and removing it.
    ///
    /// Triggers distillation but preserves the JSONL history file.
    /// Returns an error if the session does not exist.
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        let handle = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        // Send Close signal; ignore errors (session may have already stopped)
        let _ = handle.inbound_tx.send(SessionMessage::Close).await;

        // Clean up per-session workspace mappings
        self.session_workspaces.remove(session_id);
        self.pending_workspaces.remove(session_id);
        self.urgent_stops.remove(session_id);

        tracing::info!(session_id = %session_id, "SessionManager: closed session");
        Ok(())
    }

    /// Send a message to a specific session.
    ///
    /// Returns an error if the session does not exist or the channel is closed.
    /// When the channel is closed (e.g. the SessionTask panicked or was evicted
    /// without cleanup), the dead handle is auto-removed so subsequent calls
    /// get a clean "Session not found" instead of "channel closed".
    pub fn send_to_session(&mut self, session_id: &str, msg: SessionMessage) -> Result<()> {
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        if let Err(_send_err) = handle.send(msg) {
            // Channel closed — the SessionTask has died (panic / eviction race).
            // Auto-remove the stale handle so the next attempt gets a clean
            // "Session not found" error instead of "channel closed".
            let was_finished = handle.join_handle.is_finished();
            self.sessions.remove(session_id);
            self.urgent_stops.remove(session_id);
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

    /// Apply MCP server configuration changes from Gateway RuntimeConfigUpdate.
    ///
    /// Connects to (or disconnects from) MCP servers and updates:
    ///   - `self.mcp_tools` — the tool wrappers for dispatch
    ///   - `self.config.full_tool_specs` — LLM-facing tool definitions
    ///   - `self.config.tool_definitions` — current active tool definitions
    ///
    /// When `configs` is `Some(vec![])`, all MCP servers are disconnected.
    /// Apply pre-connected MCP results (without performing the connection IO).
    ///
    /// This is used for startup MCP auto-connect where the actual connection
    /// is performed in a background task and results are applied asynchronously
    /// when ready — so the Gateway message loop can start immediately without
    /// blocking on MCP timeouts.
    pub fn apply_mcp_connection_result(
        &mut self,
        registry: Arc<McpRegistry>,
        wrappers: Vec<McpToolWrapper>,
        _specs: Vec<(String, serde_json::Value)>,
        failures: Vec<McpConnectionFailure>,
    ) {
        use acowork_core::tools::traits::Tool;

        // Store the registry in the MCP manager
        self.mcp_manager.set_registry(registry);

        // Store MCP tool wrappers (Arc<dyn Tool>) for dispatch
        let mcp_tool_arcs: Vec<Arc<dyn Tool>> = wrappers
            .into_iter()
            .map(|w| Arc::new(w) as Arc<dyn Tool>)
            .collect();
        self.mcp_tools = Some(mcp_tool_arcs.clone());

        // Push MCP tools to all existing sessions
        self.broadcast(SessionMessage::UpdateMcpTools {
            mcp_tools: Some(mcp_tool_arcs),
        });

        // Update full_tool_specs to include MCP tool specs
        self.rebuild_full_tool_specs_with_mcp();

        // NOTE: McpRegistry::connect_all() already logs server/tool counts.
        // We log a summary here for the SessionManager context.
        let server_count = self
            .mcp_manager
            .registry()
            .map(|r| r.server_count())
            .unwrap_or(0);
        tracing::info!(
            server_count,
            tool_count = self.mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
            failure_count = failures.len(),
            "SessionManager: MCP servers applied (async background connect)"
        );

        // Inject system notification for connection failures
        if !failures.is_empty() {
            let failure_lines: Vec<String> = failures
                .iter()
                .map(|f| format!("- Server \"{}\": {}", f.server_name, f.error_message))
                .collect();
            let notification = format!(
                "MCP server connection failed:\n{}\n\n\
You are an AI agent. If any of the above MCP servers require dependencies \
that need to be installed, use your shell tools to install them. \
After installation, ask the user to re-enable the MCP server.",
                failure_lines.join("\n")
            );
            tracing::warn!(
                failure_count = failures.len(),
                notification_len = notification.len(),
                "SessionManager: broadcasting MCP connection failure notification"
            );
            self.broadcast(SessionMessage::SystemNotification {
                content: notification,
            });
        }
    }

    /// When `configs` is `Some(non_empty)`, MCP servers are (re)connected.
    pub async fn apply_mcp_servers(
        &mut self,
        configs: Vec<acowork_core::protocol::McpServerConfigDef>,
    ) {
        use acowork_core::tools::traits::Tool;

        if configs.is_empty() {
            tracing::info!("SessionManager: disconnecting all MCP servers");
            // Disconnect existing MCP connections to release resources
            self.mcp_manager.disconnect().await;
            self.mcp_tools = None;
            // Notify all sessions that MCP tools are gone
            self.broadcast(SessionMessage::UpdateMcpTools { mcp_tools: None });
            // Rebuild full_tool_specs without MCP tools
            self.rebuild_full_tool_specs_with_mcp();
            return;
        }

        // Disconnect previous MCP connections before connecting new ones
        self.mcp_manager.disconnect().await;

        let (registry, wrappers, _specs, failures) = self.mcp_manager.connect(&configs).await;

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

        tracing::info!(
            server_count = registry.server_count(),
            tool_count = registry.tool_count(),
            failure_count = failures.len(),
            "SessionManager: MCP servers applied"
        );

        // Inject system notification for connection failures so the LLM can self-heal
        if !failures.is_empty() {
            let failure_lines: Vec<String> = failures
                .iter()
                .map(|f| format!("- Server \"{}\": {}", f.server_name, f.error_message))
                .collect();
            let notification = format!(
                "MCP server connection failed:\n{}\n\n\
You are an AI agent. If any of the above MCP servers require dependencies \
that need to be installed, use your shell tools to install them. \
After installation, ask the user to re-enable the MCP server.",
                failure_lines.join("\n")
            );
            tracing::warn!(
                failure_count = failures.len(),
                notification_len = notification.len(),
                "SessionManager: broadcasting MCP connection failure notification"
            );
            self.broadcast(SessionMessage::SystemNotification {
                content: notification,
            });
        }
    }

    /// Rebuild `full_tool_specs` by merging the original built-in specs with
    /// any currently connected MCP tool specs.
    fn rebuild_full_tool_specs_with_mcp(&mut self) {
        // Start from the original built-in tool specs (stored at init time).
        // We store these separately to avoid losing them on rebuild.
        let mut specs = self.config.full_tool_specs.clone();

        // Remove any previous MCP entries (prefixed with "mcp_")
        specs.retain(|(name, _)| !name.starts_with("mcp_"));

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

    /// Update the global provider list from a ProviderListUpdate push.
    ///
    /// Updates the shared AgentCore's `global_provider_list`, version, and
    /// `provider_key_vault`. No per-session broadcast is needed — sessions
    /// query the shared cache on demand via
    /// [`AgentCore::get_provider`] / [`AgentCore::get_model_capabilities`].
    pub fn update_global_provider_list(
        &mut self,
        provider_list: Vec<acowork_core::protocol::ProviderListItem>,
        provider_list_version: u64,
        provider_key_vault: Vec<acowork_core::protocol::ProviderKeyEntry>,
    ) {
        tracing::info!(
            provider_count = provider_list.len(),
            version = provider_list_version,
            key_count = provider_key_vault.len(),
            "SessionManager: updating global provider list"
        );

        // The shared `core` is wrapped in `Arc<AgentCore>` and may be cloned
        // by SessionTasks; mutate `provider_compact_models` and the version
        // counter only when we are the sole owner. The provider_list and
        // key vault live behind `Arc<RwLock<...>>` and can be updated
        // regardless of refcount.
        if let Some(c) = Arc::get_mut(&mut self.core) {
            c.provider_compact_models.clear();
            for provider in &provider_list {
                c.provider_compact_models
                    .insert(provider.id.clone(), provider.compact_model.clone());
            }
            c.provider_list_version = provider_list_version;
        } else {
            tracing::warn!(
                "SessionManager: AgentCore Arc has multiple owners; \
                 provider_compact_models / provider_list_version not updated. \
                 Sessions will still see new provider_list + key vault via shared RwLock."
            );
        }

        // Replace the shared global provider list (live read-view for sessions).
        {
            let mut list = self.core.global_provider_list.write().unwrap();
            *list = provider_list;
        }

        // Notify all active sessions so they emit an updated state to the frontend.
        // The shared global_provider_list on AgentCore is already updated above,
        // so sessions can query it on demand via get_model_capabilities.
        let session_count = self.broadcast(SessionMessage::ProviderListUpdated).len();
        tracing::debug!(
            session_count = %session_count,
            "ProviderListUpdated: broadcast to all active sessions"
        );

        // Replace the shared key vault (in-memory only, never persisted).
        {
            let mut vault = self.core.provider_key_vault.write().unwrap();
            vault.clear();
            for entry in provider_key_vault {
                vault.insert(entry.provider_id, entry.api_key);
            }
        }
    }

    /// Route a model switch to a specific session (ADR-012: per-session model).
    ///
    /// Only sends the ModelSwitch message to the targeted session.
    /// Model persistence is handled by the SessionTask itself (via
    /// `ConversationSession::update_model_provider`).
    pub fn route_model_switch(
        &mut self,
        session_id: &str,
        model: String,
        provider: Option<String>,
    ) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            model = %model,
            provider = ?provider,
            "SessionManager: routing model_switch to session (ADR-012: per-session)"
        );
        self.send_to_session(session_id, SessionMessage::ModelSwitch { model, provider })
    }

    /// Route per-session reasoning effort override to the target session.
    ///
    /// Sends a ReasoningEffort message to the session task, which updates
    /// the SessionState and persists the change.
    pub fn route_reasoning_effort(
        &mut self,
        session_id: &str,
        effort: String,
    ) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            effort = %effort,
            "SessionManager: routing reasoning_effort to session"
        );
        self.send_to_session(session_id, SessionMessage::ReasoningEffort { effort })
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
            "SessionManager: search config received (keys held in memory, not cached)"
        );
    }

    /// Update user identity from Gateway UserProfileUpdate push.
    ///
    /// Formats the `UserProfile` into an `identity_context` text block
    /// and broadcasts it to all active sessions via their ContextBuilder.
    pub fn update_user_identity(&mut self, profile: Option<acowork_core::protocol::UserProfile>) {
        let identity_context = profile.as_ref().map(format_user_profile_context);
        tracing::info!(
            has_profile = profile.is_some(),
            ctx_len = identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
            "SessionManager: updating user identity"
        );
        self.config.identity_context = identity_context.clone();
        // Broadcast updated identity to all active sessions
        for handle in self.sessions.values() {
            let _ = handle.send(SessionMessage::UpdateIdentityContext {
                identity_context: identity_context.clone(),
            });
        }
    }

    /// Handle EmbeddingConfigUpdate from Gateway.
    ///
    /// When the user switches the active embedding model, the Gateway pushes
    /// this update to all running Runtimes. The Runtime rebuilds its
    /// FallbackEmbeddingProvider chain with the new ONNX provider as the
    /// first entry, following the same cache + broadcast pattern as
    /// `update_llm_config` (ADR-012).
    pub fn handle_embedding_config_update(
        &mut self,
        embed_endpoint: String,
        embed_model_id: String,
        embed_dimension: usize,
    ) {
        tracing::info!(
            endpoint = %embed_endpoint,
            model_id = %embed_model_id,
            dimension = embed_dimension,
            "SessionManager: received EmbeddingConfigUpdate"
        );

        // Cache the config for persistence and new session construction.
        self.pending_embed_config = Some(PendingEmbedConfig {
            embed_endpoint: embed_endpoint.clone(),
            embed_model_id: embed_model_id.clone(),
            embed_dimension,
        });

        // Broadcast to all existing sessions so they rebuild their
        // embedding provider in-place (same pattern as UpdateProvider).
        for (sid, handle) in &self.sessions {
            if handle
                .send(SessionMessage::UpdateEmbedConfig {
                    embed_endpoint: embed_endpoint.clone(),
                    embed_model_id: embed_model_id.clone(),
                    embed_dimension,
                })
                .is_err()
            {
                tracing::warn!(
                    session_id = %sid,
                    "Failed to send UpdateEmbedConfig to session (channel closed)"
                );
            }
        }
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

    /// Get the session state snapshot for a specific session.
    ///
    /// Returns the most recent `SessionStateSnapshot` written by the session's
    /// AgentLoop, or `None` if the session is not found or no state has been
    /// emitted yet (session just created before the first `emit_session_state`).
    pub fn snapshot_session_state(
        &self,
        session_id: &str,
    ) -> Option<crate::agent::session_state::SessionStateSnapshot> {
        self.sessions
            .get(session_id)
            .and_then(|handle| handle.snapshot())
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

    /// Access the shared core's manifest (ADR-012: for per-session model defaults).
    pub fn manifest(&self) -> &acowork_core::AgentManifest {
        self.core.manifest()
    }

    /// Get the name of the first available provider from the global cache.
    /// Used for budget queries in the Gateway loop and ConfigSnapshot.
    /// Returns an empty string if no providers are configured.
    pub fn provider_name(&self) -> String {
        let list = self.core.global_provider_list.read().unwrap();
        list.first().map(|p| p.id.clone()).unwrap_or_default()
    }

    /// Per-session model is owned by SessionState, not SessionManager.
    ///
    /// Returns the model from the most recently active session's snapshot.
    /// Falls back to the first model in `global_provider_list` (which mirrors
    /// the startup model selection in `agent_init.rs`). Returns `None` only
    /// when no provider is configured at all.
    pub fn current_model_name(&self) -> Option<String> {
        // 1. Try the most recently active session that has a model set.
        let from_session = self
            .sessions
            .values()
            .filter_map(|handle| {
                let snap = handle.snapshot_slot.read().ok()?;
                let model = snap.as_ref()?.model.clone()?;
                let ts = *handle.last_active_at.lock().ok()?;
                Some((ts, model))
            })
            .max_by_key(|(ts, _)| *ts)
            .map(|(_, model)| model);

        if from_session.is_some() {
            return from_session;
        }

        // 2. Fall back to the first model from the provider list (startup default).
        let list = self.core.global_provider_list.read().unwrap();
        list.iter()
            .flat_map(|p| p.models.iter())
            .next()
            .map(|m| m.id.clone())
    }

    /// Returns the (model, provider) pair from the most recently active session.
    ///
    /// Unlike [`current_model_name`] which only returns the model, this method
    /// atomically retrieves both model and provider from the **same** session
    /// snapshot, preventing cross-contamination when different providers
    /// expose identically-named models (e.g. "gpt-4" in both OpenAI and Azure).
    ///
    /// Falls back to the first (model, provider) from `global_provider_list`.
    /// Returns `(None, None)` only when no provider is configured at all.
    pub fn current_model_and_provider(&self) -> (Option<String>, Option<String>) {
        // 1. Try the most recently active session that has both model and provider set.
        if let Some((model, provider)) = self
            .sessions
            .values()
            .filter_map(|handle| {
                let snap = handle.snapshot_slot.read().ok()?;
                let model = snap.as_ref()?.model.clone()?;
                let provider = snap.as_ref()?.provider.clone()?;
                let ts = *handle.last_active_at.lock().ok()?;
                Some((ts, model, provider))
            })
            .max_by_key(|(ts, _, _)| *ts)
            .map(|(_, model, provider)| (model, provider))
        {
            return (Some(model), Some(provider));
        }

        // 2. Fall back to the first provider+model from the global provider list.
        let list = self.core.global_provider_list.read().unwrap();
        let provider = list.first().map(|p| p.id.clone());
        let model = list
            .first()
            .and_then(|p| p.models.first())
            .map(|m| m.id.clone());
        (model, provider)
    }

    /// Access the Grafeo memory store from the shared core.
    /// Returns None if the memory store was not initialized.
    pub(crate) fn memory_store(&self) -> Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>> {
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

    /// Extract the target session ID from request params.
    ///
    /// Every message MUST carry an explicit `session_id` — the backend is
    /// stateless with respect to "which session is current".  Returns an
    /// error when `session_id` is missing or empty so the caller can
    /// reject the message cleanly.
    pub fn require_session_id(params: &serde_json::Value) -> Result<String> {
        params
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                RuntimeError::Config(
                    "Missing or empty session_id parameter — every message must carry a session_id"
                        .to_string(),
                )
            })
    }

    /// Evict idle sessions from memory.
    ///
    /// A session is evicted when ALL of the following conditions are met:
    /// 1. Its status is `Idle` (not Streaming/WaitingApproval/Paused)
    /// 2. It has been idle for longer than `idle_timeout`
    ///
    /// Eviction destroys the in-memory SessionTask but leaves the JSONL
    /// file on disk. The session can be re-activated later via lazy resume
    /// in the `activate_session` handler.
    pub async fn evict_idle_sessions(&mut self, idle_timeout: std::time::Duration) {
        let mut to_evict = Vec::new();

        for (session_id, handle) in &self.sessions {
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
                let _ = handle.inbound_tx.send(SessionMessage::Close).await;
                self.urgent_stops.remove(session_id);
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
    /// conversation file (when one exists). When a `resolver` has been
    /// set via [`set_resolver`], also sends `SetWorkDir` so that
    /// `AgentCore::current_work_dir` is updated for tool execution.
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
        // When resolver is available, also send SetWorkDir so that
        // AgentCore::current_work_dir is updated for tool execution.
        // Without this, the session would keep using the default agent_home
        // path until an explicit SetSessionWorkspace message arrives from
        // the Gateway.
        if let Some(ref resolver) = self.resolver {
            let (resolved_path, _is_home) = {
                let guard = resolver.read().unwrap();
                self.current_dir_for(session_id, &guard)
            };
            if let Some(handle) = self.sessions.get(session_id) {
                let _ = handle.send(SessionMessage::SetWorkDir {
                    path: resolved_path,
                });
            }
        }
    }

    /// Set the session workspace and also send the resolved workspace path
    /// for tool execution. This is the preferred method when the resolver
    /// is available.
    pub fn set_session_workspace_with_resolver(
        &mut self,
        session_id: &str,
        workspace_id: &str,
        resolver: &WorkspaceResolver,
    ) {
        self.set_session_workspace(session_id, workspace_id);
        // Resolve and send the workspace path for tool execution
        let (resolved_path, _is_home) = self.current_dir_for(session_id, resolver);
        if let Some(handle) = self.sessions.get(session_id) {
            let _ = handle.send(SessionMessage::SetWorkDir {
                path: resolved_path,
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
    pub fn current_dir_for(
        &self,
        session_id: &str,
        resolver: &WorkspaceResolver,
    ) -> (String, bool) {
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
    /// Also reads and sends workspace prompt file content (CLAUDE.md / AGENTS.md).
    ///
    /// The shared `WorkspaceResolver` (set via `set_resolver()`) is the
    /// single source of truth; this method acquires the read lock internally
    /// so callers don't need to manage it.
    pub fn update_session_workspace_context(&self, session_id: &str) {
        let resolver = self
            .resolver
            .as_ref()
            .expect("set_resolver must be called before any workspace context update");
        let resolver_guard = resolver.read().unwrap();
        let ws_id = self.session_workspace_id(session_id);
        let context_text = format_workspace_context_for_session(&resolver_guard, ws_id);
        let prompt_file_content = resolver_guard.read_prompt_file(ws_id);
        if let Some(handle) = self.sessions.get(session_id) {
            let has_prompt_file = prompt_file_content.is_some();
            let _ = handle.send(SessionMessage::UpdateWorkspaceContext { context_text });
            let _ = handle.send(SessionMessage::SetWorkspacePromptFile {
                content: prompt_file_content,
            });
            tracing::info!(
                session_id = %session_id,
                workspace_id = %ws_id,
                has_prompt_file,
                "SessionManager: sent per-session workspace context and prompt file"
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
            self.pending_workspaces
                .insert(sid.clone(), old_ws_id.clone());
            self.session_workspaces
                .insert(sid.clone(), "__agent_home__".to_string());
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

    /// Fire the urgent_stop notify for a specific session.
    ///
    /// Wakes the target session's tokio::select! branches (LLM streaming,
    /// tool execution) immediately, without waiting for the 500ms poll
    /// interval. Other sessions are completely unaffected.
    ///
    /// This is a no-op in standalone mode (where urgent_stop is None).
    pub(crate) fn fire_urgent_stop(&self, session_id: &str) {
        if let Some(urgent) = self.urgent_stops.get(session_id) {
            urgent.notify_waiters();
            tracing::info!(session_id = %session_id, "SessionManager: urgent_stop fired");
        } else {
            tracing::debug!(session_id = %session_id, "SessionManager: fire_urgent_stop — session not found (may have already closed)");
        }
    }

    /// Fire the urgent_stop notify for ALL active sessions.
    ///
    /// Used by EnableDebugMode to cancel in-flight work across all sessions
    /// so they restart with debug capabilities.
    pub(crate) fn fire_urgent_stop_all(&self) {
        let count = self.urgent_stops.len();
        for urgent in self.urgent_stops.values() {
            urgent.notify_waiters();
        }
        tracing::info!(
            session_count = count,
            "SessionManager: urgent_stop fired (all sessions)"
        );
    }

    /// Initialize debug mode at runtime (called when Gateway pushes EnableDebugMode).
    ///
    /// Starts a DebugProtocolServer on `debug_port` and stores the resulting
    /// controller, event sender, and notify handles. Then pushes the handles
    /// to all existing sessions via `SessionMessage::EnableDebugMode` so they
    /// can start emitting debug events immediately, without a restart.
    pub async fn enable_debug_mode(&mut self, debug_port: u32) {
        // Avoid double-init: if debug handles are already set, skip.
        if self.runtime_debug_handles.is_some() {
            tracing::warn!(
                debug_port = debug_port,
                "enable_debug_mode: debug handles already set, skipping"
            );
            return;
        }

        let port = debug_port as u16;
        let debug_server =
            crate::debug::server::DebugProtocolServer::new(port, self.debug_controllers.clone());
        let debug_event_tx = debug_server.start().await;

        // Create debug controllers for ALL existing sessions and register
        // them in the shared debug_controllers map. New sessions created
        // while debug mode is active register their own controllers at
        // creation time via pending_debug_handles.
        {
            let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
            let mut controllers = self.debug_controllers.write().await;
            for sid in session_ids {
                let debug_ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
                controllers.insert(sid, debug_ctrl);
            }
        }

        // Build the shared DebugHandles template from the first per-session
        // controller. The event_tx is shared across all sessions; notify handles
        // come from a per-session controller so the debug server's notify_one()
        // calls (which target per-session controllers) align with SessionTask
        // waiters. The debug_ctrl in this template is only a fallback —
        // push_debug_mode_to_existing_sessions and create_session both construct
        // per-session DebugHandles using each session's own controller.
        let template_handles = {
            let controllers = self.debug_controllers.read().await;
            if let Some(first_ctrl) = controllers.values().next() {
                let guard = first_ctrl.lock().await;
                DebugHandles {
                    debug_ctrl: first_ctrl.clone(),
                    debug_event_tx: debug_event_tx.clone(),
                    rewind_notify: guard.rewind_notify_handle(),
                    resume_notify: guard.resume_notify_handle(),
                }
            } else {
                // No sessions exist yet — create a minimal controller just for
                // its notify handles. Its iteration/phase state will never be read.
                let ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
                let ctrl_for_lock = ctrl.clone();
                let (rw, rs) = {
                    let guard = ctrl_for_lock.lock().await;
                    (guard.rewind_notify_handle(), guard.resume_notify_handle())
                };
                DebugHandles {
                    debug_ctrl: ctrl,
                    debug_event_tx: debug_event_tx.clone(),
                    rewind_notify: rw,
                    resume_notify: rs,
                }
            }
        };
        self.runtime_debug_handles = Some(template_handles);

        tracing::info!(
            port = port,
            "enable_debug_mode: debug server started, handles stored for future sessions"
        );

        // Push debug handles to all existing sessions so their AgentCore
        // gets debug_ctrl/debug_event_tx injected. Without this, existing
        // sessions would continue without debug instrumentation while the
        // DebugProtocolServer would show iteration:0 forever.
        self.push_debug_mode_to_existing_sessions().await;
    }

    /// Push EnableDebugMode to every existing session so they inject the
    /// debug handles into their AgentCore without a restart.
    ///
    /// Each session receives its own per-session `DebugController` (stored
    /// in `self.debug_controllers`) so that the AgentLoop's state updates
    /// are visible to the `DebugProtocolServer` via `getState`. The notify
    /// handles (rewind/resume) also come from the per-session controller so
    /// that the debug server's `notify_one()` calls reach the correct waiter.
    async fn push_debug_mode_to_existing_sessions(&self) {
        let Some(ref handles) = self.runtime_debug_handles else {
            return;
        };
        let controllers = self.debug_controllers.read().await;
        for (sid, session_handle) in &self.sessions {
            // Use the per-session controller registered in debug_controllers,
            // NOT the global handles.debug_ctrl. The DebugProtocolServer reads
            // from debug_controllers for getState, so the AgentLoop must write
            // to the same instance.
            let per_session_ctrl = controllers
                .get(sid)
                .cloned()
                .unwrap_or_else(|| handles.debug_ctrl.clone());
            let ctrl_ptr = Arc::as_ptr(&per_session_ctrl) as *const ();
            tracing::debug!(
                session_id = %sid,
                ctrl_ptr = ?ctrl_ptr,
                found_in_map = controllers.contains_key(sid),
                "push_debug_mode: per-session controller resolved"
            );
            // Extract notify handles from the per-session controller.
            // The debug server calls ctrl.resume_notify.notify_one() on this
            // same controller instance, so SessionTask must wait on the same
            // Notify arcs.
            let (per_rewind, per_resume) = {
                let guard = per_session_ctrl.lock().await;
                (guard.rewind_notify_handle(), guard.resume_notify_handle())
            };
            let per_session_handles = DebugHandles {
                debug_ctrl: per_session_ctrl,
                debug_event_tx: handles.debug_event_tx.for_session(sid.clone()),
                rewind_notify: per_rewind,
                resume_notify: per_resume,
            };

            // Bypass path: write debug handles into pending_debug_handles so
            // that check_and_apply_pending_debug() inside execute_single_iteration
            // can pick them up EVEN when the SessionTask's message loop is blocked
            // inside agent_loop.run(). Without this, EnableDebugMode just sits in
            // the inbound channel queue and the AgentLoop never sees debug_ctrl.
            {
                let mut pending = session_handle.pending_debug_handles.lock().await;
                *pending = Some(per_session_handles.clone());
                tracing::debug!(
                    session_id = %sid,
                    ctrl_ptr = ?ctrl_ptr,
                    "push_debug_mode: handles written to pending_debug_handles (bypass)"
                );
            }

            let msg = SessionMessage::EnableDebugMode(per_session_handles);
            if session_handle.inbound_tx.send(msg).await.is_err() {
                tracing::warn!(
                    session_id = %sid,
                    "SessionManager: failed to push EnableDebugMode to session (channel closed)"
                );
            } else {
                tracing::info!(
                    session_id = %sid,
                    "SessionManager: pushed EnableDebugMode to existing session"
                );
            }
        }
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
pub(crate) fn format_user_profile_context(profile: &acowork_core::protocol::UserProfile) -> String {
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
    
    

    #[allow(dead_code)]
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
    fn test_overrides_merge() {
        let mut ov = RuntimeConfigOverrides::default();
        ov.merge(Some(100), None, None, None, None);
        assert!(!ov.is_empty());
        assert_eq!(ov.max_output_tokens, Some(100));

        // Re-merge with Some replaces
        ov.merge(Some(200), None, None, None, None);
        assert_eq!(ov.max_output_tokens, Some(200));

        // None preserves
        ov.merge(None, None, None, None, None);
        assert_eq!(ov.max_output_tokens, Some(200));
    }

    // ── require_session_id ─────────────────────────────────────────────

    #[test]
    fn test_require_session_id_valid() {
        let params = serde_json::json!({ "session_id": "test-sid" });
        assert_eq!(
            SessionManager::require_session_id(&params).unwrap(),
            "test-sid"
        );
    }

    #[test]
    fn test_require_session_id_missing() {
        let params = serde_json::json!({});
        assert!(SessionManager::require_session_id(&params).is_err());
    }

    #[test]
    fn test_require_session_id_empty() {
        let params = serde_json::json!({ "session_id": "" });
        assert!(SessionManager::require_session_id(&params).is_err());
    }
}
