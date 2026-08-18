//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acowork_core::Budget;
use acowork_core::protocol::ProtocolType;
use acowork_core::protocol::{SearchKeyEntry, SearchProviderListItem};
use acowork_core::tools::traits::Tool;
use futures_util::FutureExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::loop_::SessionChunkEvent;
use crate::agent::session::session_handle::SessionHandle;
use crate::agent::session::session_task::{SessionMessage, SessionTask};
use crate::agent::session_state::{SessionState, SessionStatus, SharedLatestSession, SharedSessionSnapshots};
use crate::cancellation::CancelHandle;
use crate::config::DEFAULT_TEMPERATURE;
use crate::conversation::{ConversationSession, read_session_meta};
use crate::debug::controller::DebugController;
use crate::error::{Result, RuntimeError};
use crate::agent_config::AgentConfig;
use crate::tools::mcp_manager::McpConnectionFailure;
use crate::tools::mcp_manager::McpManager;
use crate::tools::workspace_resolver::{WorkspaceResolver, format_workspace_context_for_session};
use acowork_mcp::client::McpRegistry;
use acowork_mcp::wrapper::McpToolWrapper;

/// Session lifecycle state observable from outside the manager (ADR-038).
///
/// Source of truth:
/// - `Active`: session is currently loaded in the in-memory `sessions` map.
/// - `Closed`: JSONL + meta files exist on disk, but the session is not in memory.
/// - `NotFound`: neither in memory nor on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleState {
    NotFound,
    Closed,
    Active,
}

/// Outcome of an explicit `open()` call (ADR-038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOpenOutcome {
    /// Session was already in memory; this call was a no-op.
    AlreadyActive,
    /// Session was on disk (Closed); we just resumed it into memory.
    ResumedFromDisk,
}

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
    /// ADR-021: Single chunk sender for control events.
    /// When set, each session's AgentLoop forwards control events here
    /// so the caller can relay them to Gateway.
    pub chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Full tool specs (name, schema) for ALL registered built-in tools.
    /// Stored so that tool definitions can be hot-rebuilt when `active_tools`
    /// changes without requiring access to the ToolRegistry (which is behind Arc).
    pub full_tool_specs: Vec<(String, serde_json::Value)>,
    /// Identity context string injected by Gateway for ContextBuilder.
    pub identity_context: Option<String>,
    /// LLM protocol type derived from models.dev (used for image token estimation)
    pub protocol_type: ProtocolType,

    /// Shared session snapshot map for the Runtime HTTP pull API.
    /// When `Some`, SessionManager registers each session's snapshot Arc
    /// here on creation and removes it on session destruction, so the
    /// Runtime HTTP server can serve `GET /sessions/{sid}/state`.
    pub session_snapshots: Option<SharedSessionSnapshots>,

    /// Shared latest session Arc for the Runtime HTTP pull API.
    /// When `Some`, SessionManager writes to this on every session creation
    /// and startup scan completion, so the Runtime HTTP server's
    /// `GET /sessions/latest` always returns the authoritative answer
    /// without file-system scanning.
    pub latest_session: Option<SharedLatestSession>,

    /// ADR-047: Shared session config map for `SessionConfigService`.
    /// When `Some`, SessionManager registers each session's
    /// `Arc<ConversationSession>` here on creation and removes it on
    /// session destruction, so the HTTP server can serve
    /// `GET/PUT /sessions/{sid}/config` without going through the
    /// serial inference queue.
    pub session_configs: Option<crate::usecases::SharedSessionConfigs>,
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
            full_tool_specs: Vec::new(),
            identity_context: None,
            protocol_type: ProtocolType::default(),
            session_snapshots: None,
            latest_session: None,
            session_configs: None,
        }
    }
}

/// Accumulated runtime config overrides pushed by Gateway via
/// `RuntimeConfigUpdate`. Applied on top of the shared `AgentCore` template
/// each time a new session is spawned, so config changes remain effective
/// for sessions created *after* the push (not only for sessions that were
/// already alive when the push arrived).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RuntimeConfigOverrides {
    pub max_output_tokens: Option<u64>,
    pub max_iterations: Option<u32>,
    pub temperature: Option<f32>,
    pub context_window: Option<u64>,
    pub system_prompt_override: Option<String>,
    pub shell_approval_threshold: Option<String>,
    pub approval_timeout_secs: Option<u64>,
    /// ADR-052: Whether context_retrieve + context_abandon tools are registered.
    /// `None` falls through to `true` (default enabled).
    /// Hot-reloadable: a `RuntimeConfigUpdate.tool_compression_enabled` push
    /// from Gateway flows through
    /// `SessionManager::apply_runtime_config_override` -> the shared
    /// `AgentCore` template (so future sessions inherit it) and every
    /// active SessionTask's `ContextBuilder.tool_definitions` (so the LLM
    /// sees the new set on the next `build_chat_request`). ADR-052 §3.5.
    pub tool_compression_enabled: Option<bool>,
}

impl RuntimeConfigOverrides {
    /// Returns true when no override value has been set.
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.temperature.is_none()
            && self.context_window.is_none()
            && self.system_prompt_override.is_none()
            && self.shell_approval_threshold.is_none()
            && self.approval_timeout_secs.is_none()
            && self.tool_compression_enabled.is_none()
    }

    /// Merge in a newer push. `Some` values replace; `None` preserves the
    /// previously cached override.
    pub fn merge(&mut self, other: &Self) {
        if other.max_output_tokens.is_some() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if other.max_iterations.is_some() {
            self.max_iterations = other.max_iterations;
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.context_window.is_some() {
            self.context_window = other.context_window;
        }
        if other.system_prompt_override.is_some() {
            self.system_prompt_override = other.system_prompt_override.clone();
        }
        if other.shell_approval_threshold.is_some() {
            self.shell_approval_threshold = other.shell_approval_threshold.clone();
        }
        if other.approval_timeout_secs.is_some() {
            self.approval_timeout_secs = other.approval_timeout_secs;
        }
        if other.tool_compression_enabled.is_some() {
            self.tool_compression_enabled = other.tool_compression_enabled;
        }
    }

    /// Apply this override onto an `AgentConfig` for persistence to disk.
    /// `Some` values are written; `None` preserves the on-disk value
    /// (so a partial push does not clobber unrelated fields).
    ///
    /// This is the **single source of truth** for the
    /// `RuntimeConfigOverrides → AgentConfig` mapping. Both the boot path
    /// (read-modify-write via `apply_runtime_config_override`) and the
    /// live-edit path (`RuntimeConfigUpdate` push from Gateway) should
    /// funnel through here to avoid schema-drift when new override
    /// fields are added.
    pub fn apply_to(&self, cfg: &mut AgentConfig) {
        if let Some(v) = self.max_output_tokens {
            cfg.max_output_tokens = Some(v);
        }
        if let Some(v) = self.max_iterations {
            cfg.max_iterations = Some(v);
        }
        if let Some(v) = self.temperature {
            cfg.temperature = Some(v);
        }
        if let Some(v) = self.context_window {
            cfg.context_window = Some(v);
        }
        if let Some(v) = &self.system_prompt_override {
            cfg.system_prompt_override = Some(v.clone());
        }
        if let Some(ref v) = self.shell_approval_threshold {
            cfg.shell_approval_threshold = Some(v.clone());
        }
        if let Some(v) = self.approval_timeout_secs {
            cfg.approval_timeout_secs = Some(v);
        }
        if let Some(v) = self.tool_compression_enabled {
            cfg.tool_compression_enabled = Some(v);
        }
    }
}

impl From<&AgentConfig> for RuntimeConfigOverrides {
    /// Project the subset of `AgentConfig` fields that participate in the
    /// runtime override chain. Other `AgentConfig` fields (max_sessions,
    /// avatar, etc.) are applied separately via their own code paths.
    ///
    /// Pass-through projection: every field maps 1:1 from `AgentConfig`.
    fn from(cfg: &AgentConfig) -> Self {
        Self {
            max_output_tokens: cfg.max_output_tokens,
            max_iterations: cfg.max_iterations,
            temperature: cfg.temperature,
            context_window: cfg.context_window,
            system_prompt_override: cfg.system_prompt_override.clone(),
            shell_approval_threshold: cfg.shell_approval_threshold.clone(),
            approval_timeout_secs: cfg.approval_timeout_secs,
            tool_compression_enabled: cfg.tool_compression_enabled,
        }
    }
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
    /// ADR-030 C3: Dynamic builtin tools registered via SidecarEndpointUpdate
    /// (e.g. `codebase` when LSP relay becomes ready). Stored here so that
    /// **new** sessions created after a sidecar push also inherit them -
    /// mirrors the `mcp_tools` pattern (ADR-030 review ISSUE-1 fix).
    dynamic_builtin_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
    /// MCP connection manager.
    mcp_manager: McpManager,
    /// Per-session pending workspace reference.
    /// When a session's workspace was deleted from the resolver,
    /// the session_id → old_ws_id mapping is moved here so it can be
    /// reconciled if the workspace is re-added.
    pending_workspaces: HashMap<String, String>,
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
    /// ADR-044: Per-session cancellation tokens (the *new* single source of
    /// truth for "stop this session now"). Keyed by `session_id` so external
    /// signal sources — MQTT `ControlAction::StopGeneration` dispatcher, debug
    /// server `Stop`, CLI cancel, test harness — can locate the target
    /// session's token and call `cancel(CancellationReason::...)` on it.
    ///
    /// **Phase 3 (current state)**: registered in parallel with
    /// `urgent_stops`, with the MQTT `ControlAction::StopGeneration`
    /// dispatcher now firing `cancel(CancellationReason::UserStop)` here
    /// before forwarding the inbound message — see
    /// `startup/gateway_loop.rs::dispatch_inbound`. The token is the
    /// *active* stop-path for production code; `urgent_stops` remains
    /// wired only as a rollback safety net.
    ///
    /// **Phase 4**: `urgent_stops` removed; this map is the sole stop-path
    /// index. Until then both maps stay in sync via the same lifecycle
    /// hooks (insert on session creation, remove on close/delete/eviction).
    ///
    /// ADR-044 §4.5: keys are session IDs, values are `Arc<parking_lot::Mutex<CancelHandle>>`
    /// slots — *not* `CancelHandle` clones. The slot indirection is what
    /// makes the per-request boundary safe: external dispatchers read
    /// through the Arc on every cancel call and observe the latest
    /// generation (whatever `run_inner::begin_new_request` last wrote).
    /// Storing a plain clone here would freeze the generation to whatever
    /// was in the slot at registration time — the exact bug §4.5 fixes.
    cancel_handles: HashMap<String, Arc<parking_lot::Mutex<CancelHandle>>>,
    /// Per-session committed_lines counter, shared between the writer thread
    /// (ConversationWriter) and the session's SessionCore. Each session gets its
    /// own independent counter; `committed_lines_for(session_id)` returns the
    /// count for the correct JSONL file.
    session_committed_lines: HashMap<String, Arc<std::sync::atomic::AtomicUsize>>,
    /// ADR-025: Per-session delivery cursor — the backend tracks how much
    /// data has been delivered to the frontend.  The frontend never sends
    /// coordinates; it polls with `incremental=true` and the backend uses
    /// this cursor to determine what to return.
    session_delivery_cursors: std::sync::RwLock<HashMap<String, crate::conversation::DeliveryCursor>>,
    /// Shared streaming lines map (keyed by session_id), cloned into each
    /// SessionCore and used by the HTTP handler for `read_messages_since`.
    streaming_lines: crate::conversation::StreamingStateMap,

    /// Latest session ID and title, determined during the startup session scan
    /// (by `last_active_at` descending). Set once after the background scan
    /// completes; `None` until the scan finishes or if no sessions exist.
    latest_session: SharedLatestSession,
    /// ADR-047: Shared session config map for `SessionConfigService`.
    session_configs: crate::usecases::SharedSessionConfigs,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        // Extract shared Arc before config is moved into self.config.
        let latest_session = config
            .latest_session
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(None)));

        let session_configs = config
            .session_configs
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(HashMap::new())));

        Self {
            core,
            sessions: HashMap::new(),
            config,
            runtime_overrides: RuntimeConfigOverrides::default(),
            mcp_tools: None,
            dynamic_builtin_tools: Vec::new(),
            mcp_manager: McpManager::new(),
            pending_workspaces: HashMap::new(),
            default_workspace_id: "__agent_home__".to_string(),
            resolver: None,
            runtime_debug_handles: None,
            debug_controllers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            urgent_stops: HashMap::new(),
            cancel_handles: HashMap::new(),
            session_committed_lines: HashMap::new(),
            session_delivery_cursors: std::sync::RwLock::new(HashMap::new()),
            streaming_lines: Arc::new(std::sync::RwLock::new(HashMap::new())),
            latest_session,
            session_configs,
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
        self.create_session_with_id_and_conversation(session_id, None, None)
            .await
    }

    /// Create a new session with a specific ID and optional conversation session.
    ///
    /// When `conversation` is provided, the session is initialized with JSONL
    /// persistence enabled. This is used for the initial session on cold start
    /// when a previous conversation is resumed.
    ///
    /// `committed_lines` must be the same `Arc<AtomicUsize>` that was passed to
    /// the `ConversationSession`'s writer thread. It is shared between the
    /// session's AgentCore and the background writer so that
    /// `notify_new_data_available` and HTTP poll handlers always read the
    /// correct per-session line count.
    pub async fn create_session_with_id_and_conversation(
        &mut self,
        session_id: String,
        conversation: Option<ConversationSession>,
        committed_lines: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Result<String> {
        // Read the persisted workspace_id and model/provider before the conversation
        // is moved into SessionState, so we can restore them.
        let persisted_workspace_id = conversation
            .as_ref()
            .and_then(|c| c.workspace_id())
            .map(|w| w.to_string());

        let (inbound_tx, inbound_rx) = mpsc::channel(self.config.inbound_channel_capacity);

        // Snapshot the title (and other resume-only fields) before `conversation`
        // is moved into `build_initial_session_state`. We need it after the
        // move to seed `latest_session`.
        let resumed_title = conversation.as_ref().and_then(|c| c.title());

        // ADR-047: wrap conversation in Arc so it can be shared between
        // SessionState (owned by SessionTask/AgentLoop) and SessionHandle
        // (owned by SessionManager). This enables config mutations to bypass
        // the serial inference queue.
        let conversation_arc = conversation.map(Arc::new);

        let session_state = self.build_initial_session_state(conversation_arc.clone());

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
            let (per_rewind, per_resume, per_control) = {
                let guard = ctrl.lock().await;
                (
                    guard.rewind_notify_handle(),
                    guard.resume_notify_handle(),
                    guard.control_notify_handle(),
                )
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
                control_notify: per_control,
            })
        } else {
            None
        };

        // Create per-session workspace Arcs — the single source of truth.
        // SessionCore and SessionHandle share these Arcs, so SessionManager
        // can read/write workspace state synchronously without channel delay.
        let initial_workspace = persisted_workspace_id
            .clone()
            .unwrap_or_else(|| self.default_workspace_id.clone());
        let workspace_id: Arc<std::sync::RwLock<String>> =
            Arc::new(std::sync::RwLock::new(initial_workspace.clone()));
        self.pending_workspaces.remove(&session_id);
        // Store per-session committed_lines for HTTP handler access.
        if let Some(ref cl) = committed_lines {
            self.session_committed_lines.insert(session_id.clone(), cl.clone());
        }
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
        let current_work_dir: Arc<std::sync::RwLock<Option<String>>> =
            Arc::new(std::sync::RwLock::new(Some(initial_work_dir)));

        // For sessions without a persistent conversation, create a dummy
        // committed_lines counter.  This session won't produce JSONL writes
        // (no writer thread), so the counter stays at 0 — which is accurate.
        let session_committed_lines = committed_lines
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)));

        // Extract the snapshot Arc before session_state is moved into SessionTask.
        // The snapshot is already populated with persistent data by
        // build_initial_session_state; we just need to set session_id.
        //
        // ADR-039: workspace_id is no longer mirrored into the runtime
        // snapshot — it lives in `data/meta/{session_id}.json` and is
        // broadcast through the `session_meta` MQTT channel.
        let snapshot_arc = session_state.snapshot.clone();
        {
            if let Ok(mut snap) = snapshot_arc.write() {
                snap.session_id = session_id.clone();
            }
        }

        let (mut task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.identity_context.clone(),
            self.config.protocol_type.clone(),
            self.mcp_tools.clone(),
            self.dynamic_builtin_tools.clone(),
            per_session_debug,
            pending_debug_handles.clone(),
            self.runtime_overrides.clone(),
            current_work_dir.clone(),
            session_committed_lines,
            self.streaming_lines.clone(),
        );

        // ADR-014: Create watch channel for session status
        let (status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        task.set_status_tx(status_tx);

        // Register per-session urgent_stop Notify so fire_urgent_stop()
        // only wakes this session's tokio::select! branches.
        if let Some(notify) = task.urgent_stop_notify() {
            self.urgent_stops.insert(session_id.clone(), notify);
        }

        // ADR-044 §4.5: Register the per-session Arc slot (not a `CancelHandle`
        // clone). Phase 3 external callers (MQTT dispatcher, debug server)
        // look up the slot via [`Self::cancel_handle`] and read the
        // *current* generation through `Arc::lock()` on every dispatch —
        // never a stale clone. Inserted unconditionally — the slot is
        // always allocated in `SessionCore::new`, so no `Option` unwrap is
        // needed here.
        self.cancel_handles
            .insert(session_id.clone(), task.cancel_handle_arc());

        // Spawn the session task with panic isolation.
        // catch_unwind ensures that if SessionTask::run() panics, we log the
        // panic with the session_id before the task terminates. Without this,
        // tokio::spawn silently swallows the panic and the only symptom is a
        // "Session channel closed" warning with no root cause.
        let sid = session_id.clone();
        let join_handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(task.run())
                .catch_unwind()
                .await;
            if let Err(panic_err) = result {
                let msg = panic_err
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("<non-string panic payload>");
                tracing::error!(
                    session_id = %sid,
                    panic.payload = %msg,
                    "SessionTask panicked — session will be unreachable until re-activation"
                );
            }
        });

        // ADR-047: Register the session's ConversationSession Arc in the
        // shared config map so SessionConfigService can apply config
        // changes without going through the serial inference queue.
        // Must happen before `conversation_arc` is moved into SessionHandle.
        if let Some(conv) = &conversation_arc {
            self.session_configs
                .write()
                .unwrap()
                .insert(session_id.clone(), conv.clone());
        }

        let handle = SessionHandle {
            session_id: session_id.clone(),
            inbound_tx,
            agent_inbound_tx,
            join_handle,
            status_rx,
            last_active_at: std::sync::Mutex::new(std::time::Instant::now()),
            pending_debug_handles: pending_debug_handles.clone(),
            snapshot: snapshot_arc.clone(),
            workspace_id,
            current_work_dir,
            conversation: conversation_arc,
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        // Register the session's snapshot Arc in the shared map so the
        // Runtime HTTP server can serve GET /sessions/{sid}/state.
        if let Some(ref snapshots) = self.config.session_snapshots {
            snapshots
                .write()
                .unwrap()
                .insert(session_id.clone(), snapshot_arc);
        }

        // Every newly created or resumed session becomes the "latest" by
        // definition. For resumed sessions we propagate the persisted
        // title from `ConversationSession` so `/sessions/latest` returns
        // the title the user previously set, instead of `null`. For
        // brand-new sessions the title is `None` until the first
        // user-driven rename.
        //
        // This single source of truth replaces the previous design where
        // `session_init.rs` would seed the latest-session entry first and
        // then this function would clobber it with `None`.
        self.set_latest_session(session_id.clone(), resumed_title);

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

    /// Create a new session from frontend request, handling everything in one call.
    ///
    /// Generates a session ID, creates the JSONL file with initial metadata,
    /// writes the index entry, spawns the session task, and enables
    /// notifications — all in a single atomic operation.
    ///
    /// Returns the new session ID on success.
    pub async fn create_frontend_session(
        &mut self,
        workspace_id: Option<&str>,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<String> {
        let session_id = crate::conversation::generate_session_id();
        let committed_lines = Self::new_committed_lines();

        // Create the JSONL file and index entry
        // ADR-024: read agent_config.json for max_sessions override;
        // fall back to RuntimeConfig default if absent.
        let max_sessions = crate::agent_config::load_agent_config(
            std::path::Path::new(&self.core.config.work_dir),
        )
        .unwrap_or_default()
        .unwrap_or_default()
        .max_sessions
        .unwrap_or(self.core.config.max_sessions);

        let (conv, config_rx, state_rx) = crate::conversation::ConversationSession::new(
            std::path::Path::new(&self.core.config.work_dir),
            &session_id,
            crate::conversation::SessionConfig {
                agent_id: self.core.config.agent_id.clone(),
                workspace_id: workspace_id.map(|s| s.to_string()),
                model: model.map(|s| s.to_string()),
                provider: provider.map(|s| s.to_string()),
            },
            max_sessions,
            committed_lines.clone(),
        )?;

        // ADR-043: Spawn config + state change relays.
        if let Some(chunk_tx) = self.config.chunk_tx.clone() {
            crate::startup::subsystems::spawn_config_change_relay(
                config_rx,
                chunk_tx.clone(),
                conv.clone(),
                session_id.clone(),
                // Wrap self.core in a fresh SharedAgentCore slot for the
                // relay. self.core is already fully constructed (this path
                // runs inside SessionManager which owns the Arc), so the
                // slot is immediately populated.
                std::sync::Arc::new(std::sync::RwLock::new(Some(self.core.clone()))),
            );
            crate::startup::subsystems::spawn_state_change_relay(
                state_rx,
                chunk_tx,
                conv.clone(),
                session_id.clone(),
            );
        }

        // Spawn the session task
        self.create_session_with_id_and_conversation(
            session_id.clone(),
            Some(conv),
            Some(committed_lines),
        )
        .await?;

        // ADR-035 Phase 3: EnableNotify removed — push drives all streaming,
        // no front/back suppression mechanism remains.

        Ok(session_id)
    }

    /// Build a fully-initialized SessionState for a new or resumed session.
    /// All per-session fields are set synchronously before this returns.
    /// Caller must hold an Arc<AgentCore> with global_provider_list populated.
    fn build_initial_session_state(
        &self,
        conversation: Option<Arc<ConversationSession>>,
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

            // Persist the fallback to JSONL so the session carries
            // consistent (model, provider) metadata into the next
            // `OpenSession` activation — the proto `SessionOpened` event
            // reports these fields directly to the frontend.
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
                // ADR-024: read compaction offset from per-session meta file.
                let compaction_abs = path
                    .parent()
                    .and_then(|conversations_dir| {
                        crate::conversation::read_session_meta(conversations_dir, conv.session_id())
                            .ok()
                            .and_then(|m| m.last_compaction_offset)
                    });
                match crate::agent::session::restorer::restore_history_from_jsonl(path, compaction_abs) {
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

        // Propagate temperature override to the session via the per-agent chain:
        //   runtime_overrides → agent_config.json (Layer 1) → manifest (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).
        // Always set a concrete value so the model actually receives the configured
        // temperature and the status panel can display it accurately.
        let temperature = self
            .runtime_overrides
            .temperature
            .or(self.core.temperature_override)
            .or(self.core.manifest_temperature)
            .or(Some(DEFAULT_TEMPERATURE));
        session_state.set_temperature(temperature);

        // Install the restored history *after* set_max_tokens has been applied,
        // so the lossless trim (if needed) operates against the model-correct
        // budget. Trim is the safety net for the "resumed under a smaller
        // model" case — it never invokes an LLM.
        if let Some(outcome) = restored {
            session_state.history_mut().load_restored(outcome.messages);

            // NOTE: restore does not perform placeholder compression.
            //
            // History is loaded from JSONL as-is. ADR-052 removed automatic
            // event-driven compression; tool-result compression is now
            // LLM-initiated via `context_abandon`. The JSONL stores the
            // original tool output (placeholders were in-memory only), so
            // `last_input` (restored from meta) already reflects the
            // uncompressed state — no re-compression needed at restore time.
            //
            // `fit_to_budget_lossless` remains the safety net for the
            // "resumed under a smaller model" case.
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

        // Populate the shared runtime snapshot with the initial context_usage
        // (computed from persisted tokens) so the frontend can read it via
        // fetchSessionState() immediately, without waiting for
        // emit_session_state() to run.
        //
        // ADR-039: model + provider are mirrored here as runtime-cached
        // values (not authoritative — see ADR-039 (revised)). reasoning_effort
        // and temperature are NOT mirrored — they live in
        // `data/meta/{session_id}.json` and are broadcast through the
        // `session_meta` MQTT channel.
        {
            let model_name = session_state.model().map(|s| s.to_string());
            let provider_name = session_state.provider().map(|s| s.to_string());

            // Build context_usage from persisted session tokens (if available).
            let context_usage = session_state.conversation().and_then(|conv| {
                let persisted = conv.tokens()?;
                let m = model_name.as_deref().unwrap_or("unknown");
                let caps = self.core.get_model_capabilities(m)?;
                let max_output = self.core.max_output_tokens_limit_for_model(m);
                let ctx = crate::agent::context::build_context_usage_from_persisted(
                    &caps,
                    persisted.last_input,
                    persisted.last_output,
                    max_output,
                    self.core.context_window_override,
                    Some(&persisted),
                );
                serde_json::to_string(&ctx).ok()
            });

            if let Ok(mut snap) = session_state.snapshot.write() {
                snap.model = model_name;
                snap.provider = provider_name;
                snap.context_usage = context_usage;
                // session_id is set by the caller after
                // build_initial_session_state returns.
            }
        }

        session_state
    }

    /// Close a session by ID, disabling notifications then sending Close.
    ///
    /// Triggers distillation but preserves the JSONL history file.
    /// Returns an error if the session does not exist.
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        let handle = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        // ADR-035 Phase 3: DisableNotify removed — push drives all streaming.

        // Send Close signal; ignore errors (session may have already stopped)
        let _ = handle.inbound_tx.send(SessionMessage::Close).await;

        // Clean up per-session mappings
        self.pending_workspaces.remove(session_id);
        self.urgent_stops.remove(session_id);
        self.cancel_handles.remove(session_id);
        self.session_committed_lines.remove(session_id);
        self.session_delivery_cursors.write().unwrap().remove(session_id);

        tracing::info!(session_id = %session_id, "SessionManager: closed session");
        Ok(())
    }

    /// Look up a session's **current request's** [`CancelHandle`] by `session_id`.
    ///
    /// ADR-044 §4.5: external signal sources (MQTT `ControlAction::StopGeneration`
    /// dispatcher in `startup/gateway_loop.rs`, debug server `Stop` handler,
    /// CLI cancel command, test harness) use this to obtain a handle and call
    /// `cancel(CancellationReason::UserStop { ... })` on it. Returns `None`
    /// when the session is not registered (evicted, never created, or
    /// already closed) — callers should treat that as "no session to stop"
    /// and log a warning rather than crash.
    ///
    /// **Always reads the *current* generation**: the stored value is an
    /// `Arc<parking_lot::Mutex<CancelHandle>>` slot, not a handle clone.
    /// We take a brief `parking_lot::Mutex` lock to clone the handle the
    /// slot currently holds, so callers always see whatever
    /// `run_inner::begin_new_request` last wrote — never a stale clone
    /// from session creation time.
    ///
    /// **Symmetry with the legacy `urgent_stops` map**: this is the canonical
    /// successor of the `fire_urgent_stop()`-style helper. The MQTT
    /// dispatcher calls `cancel(UserStop { source: ChatPanel { agent_id, session_id }, reason })`
    /// here at the start of its stop-message handler; the legacy `urgent_stops`
    /// map remains for incremental rollback and is removed in Phase 4.
    pub fn cancel_handle(&self, session_id: &str) -> Option<CancelHandle> {
        let slot = self.cancel_handles.get(session_id)?;
        Some(slot.lock().clone())
    }

    /// Return the `agent_id` of the owning runtime, for use as the
    /// `StopSource::ChatPanel { agent_id, ... }` payload.
    ///
    /// ADR-044 Phase 3: external callers (MQTT dispatcher, debug server,
    /// CLI) need a stable handle to identify which agent initiated the
    /// cancel when constructing [`CancellationReason::UserStop`]. Returns
    /// a borrowed `&str` from the [`AgentCore`](crate::agent::AgentCore)
    /// template so no allocation is needed at the hot path.
    pub fn agent_id(&self) -> &str {
        &self.core.config.agent_id
    }

    /// Delete a session: close the task, remove index entry, and delete JSONL file.
    ///
    /// This is an atomic operation from the caller's perspective — after this
    /// returns, the session no longer exists in memory or on disk (unless the
    /// join times out, in which case the task continues but its index entry and
    /// JSONL file are still cleaned up).
    ///
    /// Works for sessions both in memory and already evicted from memory
    /// (e.g., idle eviction, reaped handles, or previous Runtime restarts).
    /// When the session is not in `self.sessions`, the Close/join steps are
    /// skipped and only the on-disk resources are cleaned up.
    ///
    /// A 30-second timeout is applied to the session task join.  If the task
    /// does not finish within this window (e.g. distillation hangs), resources
    /// are still cleaned up and the method returns successfully — the background
    /// task's eventual `Drop` may briefly re-write the index entry, but the end
    /// result is a tombstone-free index after the next call to
    /// [`remove_session_from_index`] on a subsequent delete or prune.
    pub async fn delete_session(&mut self, session_id: &str) {
        // 1. If in memory, close the task cleanly (with timeout)
        if let Some(handle) = self.sessions.remove(session_id) {
            // Remove session snapshot from shared map (if registered)
            if let Some(ref snapshots) = self.config.session_snapshots {
                snapshots.write().unwrap().remove(session_id);
            }
            // ADR-047: Remove from shared config map.
            self.session_configs.write().unwrap().remove(session_id);
            // ADR-035 Phase 3: DisableNotify removed.
            let _ = handle.inbound_tx.send(SessionMessage::Close).await;

            // Clean up in-memory mappings
            self.pending_workspaces.remove(session_id);
            self.urgent_stops.remove(session_id);
            self.cancel_handles.remove(session_id);
            self.session_committed_lines.remove(session_id);
            self.session_delivery_cursors.write().unwrap().remove(session_id);

            // Wait for the task to finish so that Drop runs before we
            // finalize on-disk state.
            const TIMEOUT: Duration = Duration::from_secs(30);
            match tokio::time::timeout(TIMEOUT, handle.join_handle).await {
                Ok(Ok(())) => {
                    tracing::info!(session_id = %session_id, "Session task shut down cleanly");
                }
                Ok(Err(join_err)) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %join_err,
                        "Session task panicked during close"
                    );
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        session_id = %session_id,
                        timeout = ?TIMEOUT,
                        "Session task did not finish within timeout, proceeding with resource cleanup"
                    );
                }
            }
        } else {
            // Session already evicted — just clean up remaining mappings
            self.pending_workspaces.remove(session_id);
            self.urgent_stops.remove(session_id);
            self.session_committed_lines.remove(session_id);
            self.session_delivery_cursors.write().unwrap().remove(session_id);
            tracing::info!(session_id = %session_id, "Session already evicted, skipping task close");
        }

        // 2. ADR-024: remove the per-session meta file (replaces index.json update).
        let conversations_dir =
            std::path::Path::new(&self.core.config.work_dir).join("conversations");
        let meta_path = conversations_dir
            .join("meta")
            .join(format!("{}.json", session_id));
        if let Err(e) = std::fs::remove_file(&meta_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to delete session meta file"
            );
        }

        // 3. Delete the JSONL file.
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
        match std::fs::remove_file(&file_path) {
            Ok(()) => {
                tracing::info!(session_id = %session_id, "Deleted session JSONL file");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(session_id = %session_id, "Session JSONL file already deleted");
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %file_path.display(),
                    error = %e,
                    "Failed to delete session JSONL file"
                );
            }
        }

        tracing::info!(session_id = %session_id, "SessionManager: deleted session");
    }

    /// Send a message to a specific session.
    ///
    /// Returns an error if the session does not exist, the channel is full
    /// (transient backpressure — caller should retry or drop the message),
    /// or the channel is closed (SessionTask has died).
    ///
    /// **Full vs Closed distinction**: when the channel is merely full,
    /// the session handle is NOT removed — the session is healthy but
    /// experiencing backpressure. When the channel is closed (e.g. the
    /// SessionTask panicked), the stale handle IS auto-removed so
    /// subsequent calls get a clean "Session not found" instead of
    /// "channel closed".
    pub fn send_to_session(&mut self, session_id: &str, msg: SessionMessage) -> Result<()> {
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        match handle.send(msg) {
            Ok(()) => Ok(()),
            Err(send_err) => match send_err.as_ref() {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    // Transient backpressure — session is healthy, do NOT evict.
                    // The caller may retry or drop the message depending on context.
                    tracing::warn!(
                        session_id = %session_id,
                        "Session channel full (backpressure) — message dropped, session NOT evicted"
                    );
                    Err(RuntimeError::Config(format!(
                        "Session channel full: {}",
                        session_id
                    )))
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    // Channel closed — the SessionTask has died (panic / eviction race).
                    // Auto-remove the stale handle so the next attempt gets a clean
                    // "Session not found" error instead of "channel closed".
                    let was_finished = handle.join_handle.is_finished();
                    self.sessions.remove(session_id);
                    if let Some(ref snapshots) = self.config.session_snapshots {
                        snapshots.write().unwrap().remove(session_id);
                    }
                    self.session_configs.write().unwrap().remove(session_id);
                    self.urgent_stops.remove(session_id);
                    self.session_committed_lines.remove(session_id);
                    self.session_delivery_cursors.write().unwrap().remove(session_id);
                    tracing::warn!(
                        session_id = %session_id,
                        task_finished = was_finished,
                        "Session channel closed — auto-removing dead session handle"
                    );
                    Err(RuntimeError::Config(format!(
                        "Session not found: {}",
                        session_id
                    )))
                }
            },
        }
    }

    /// ADR-038: Observe the lifecycle state of a session.
    ///
    /// - `Active` if a session handle exists in the in-memory map.
    /// - `Closed` if a meta file exists on disk but no handle is loaded.
    /// - `NotFound` if neither exists.
    ///
    /// Lifecycle is now explicit (ADR-038 §3): there is no lazy-resume
    /// helper anymore — the `open_session` MQTT control command is the
    /// single entry point for transitioning Closed/NotFound → Active, and
    /// other handlers route through `dispatch_inbound` (which publishes
    /// `SessionNotOpened` when missing) or call [`Self::open`] directly.
    pub fn get_lifecycle_state(
        &self,
        session_id: &str,
        work_dir: &Path,
    ) -> SessionLifecycleState {
        if self.sessions.contains_key(session_id) {
            return SessionLifecycleState::Active;
        }
        let meta_dir = work_dir.join("conversations").join("meta");
        let meta_path = meta_dir.join(format!("{}.json", session_id));
        if meta_path.exists() {
            return SessionLifecycleState::Closed;
        }
        SessionLifecycleState::NotFound
    }

    /// ADR-038: Explicit session activation (transitions Closed/NotFound → Active).
    ///
    /// Idempotent: an already-Active session returns
    /// [`SessionOpenOutcome::AlreadyActive`] without touching disk. A Closed
    /// session is lazy-resumed from JSONL into memory, returning
    /// [`SessionOpenOutcome::ResumedFromDisk`]. A NotFound session returns an
    /// error (no meta file on disk).
    ///
    /// This replaces the implicit "lazy resume before every command" pattern.
    /// Frontend should call this explicitly via the MQTT `open_session`
    /// command; runtime dispatch paths should rely on the explicit lifecycle
    /// contract and remove their own lazy-resume fallbacks.
    pub async fn open(
        &mut self,
        session_id: &str,
        work_dir: &Path,
    ) -> Result<SessionOpenOutcome> {
        if self.sessions.contains_key(session_id) {
            return Ok(SessionOpenOutcome::AlreadyActive);
        }

        // Validate disk presence up-front so callers get a clear error
        // instead of a generic "Session not found on disk" buried inside the
        // resume path. ADR-024: meta file is the canonical "session exists" marker.
        let meta_dir = work_dir.join("conversations").join("meta");
        if !meta_dir.join(format!("{}.json", session_id)).exists() {
            return Err(RuntimeError::Config(format!(
                "Session not found on disk: {}",
                session_id
            )));
        }

        // Each resumed session gets its own committed_lines counter.
        // The writer thread (inside ConversationSession) increments it;
        // the session's AgentCore reads it via clone_for_session.
        let committed_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (conv, config_rx, state_rx) = ConversationSession::resume(work_dir, session_id, committed_lines.clone())
            .map_err(|e| {
                RuntimeError::Config(format!(
                    "Session not found on disk: {} ({})",
                    session_id, e
                ))
            })?;

        // ADR-043: Spawn config + state change relays.
        if let Some(chunk_tx) = self.config.chunk_tx.clone() {
            crate::startup::subsystems::spawn_config_change_relay(
                config_rx,
                chunk_tx.clone(),
                conv.clone(),
                session_id.to_string(),
                std::sync::Arc::new(std::sync::RwLock::new(Some(self.core.clone()))),
            );
            crate::startup::subsystems::spawn_state_change_relay(
                state_rx,
                chunk_tx,
                conv.clone(),
                session_id.to_string(),
            );
        }

        // ADR-028: merge the resumed session's persisted token totals into
        // the AgentCore counters so the live context_usage WebSocket push
        // doesn't report agent_total < session_total after a process restart.
        if let Some(t) = conv.tokens() {
            self.core.merge_token_totals((Some(t.total_input), Some(t.total_output)));
        }

        self.create_session_with_id_and_conversation(session_id.to_string(), Some(conv), Some(committed_lines))
            .await?;

        tracing::info!(
            session_id = %session_id,
            "SessionManager::open: lazy-resumed session from disk"
        );
        Ok(SessionOpenOutcome::ResumedFromDisk)
    }

    /// Read `(model, provider, last_active_at_iso)` for a session from its meta file.
    ///
    /// `last_active_at_iso` is the raw ISO-8601 string from the meta file (the
    /// Runtime does not parse it to epoch seconds). Returns `(None, None, None)`
    /// when the meta file is missing or unreadable.
    /// Used by the `open_session` handler to populate the
    /// [`crate::acowork_core::mqtt_proto::SessionOpened`] ack payload.
    pub fn session_metadata_summary(
        &self,
        session_id: &str,
        work_dir: &Path,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let meta_dir = work_dir.join("conversations");
        match read_session_meta(&meta_dir, session_id) {
            Ok(meta) => (meta.model, meta.provider, Some(meta.last_active_at)),
            Err(_) => (None, None, None),
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
    /// This performs three actions atomically from the caller's perspective:
    ///   0. Rewrite the shared `AgentCore` **template** via
    ///      [`crate::agent::agent_core::AgentCore::apply_runtime_config`]
    ///      (clone-on-write; the boot path's cold-start merge stays the
    ///      only way a new session can clone a stale template). For
    ///      `tool_compression_enabled` specifically this also
    ///      sync-gates the platform tools into / out of `builtin_tools`
    ///      and rebuilds the dispatch list on the template.
    ///   1. Merge the override into the `runtime_overrides` cache so any
    ///      session created *after* this call also picks it up (fixing the
    ///      bug where a fresh session would clone the untouched
    ///      `Arc<AgentCore>` template and silently ignore user-applied
    ///      values such as `max_iterations`).
    ///   2. Broadcast the override as `SessionMessage::UpdateRuntimeConfig`
    ///      to all active SessionTask inboxes (each task rebuilds its
    ///      `ContextBuilder.tool_definitions` and applies core flags).
    ///   3. Also deliver `InboundMessage::UserOperation(UpdateRuntimeConfig)`
    ///      via `send_inbound()` fast channel so mid-execution AgentLoops
    ///      pick up the change immediately (their `apply_user_op` syncs
    ///      `core.builtin_tools` + `all_tools` on the in-flight snapshot).
    pub fn apply_runtime_config_override(
        &mut self,
        overrides: &RuntimeConfigOverrides,
    ) -> Vec<String> {
        self.runtime_overrides.merge(overrides);

        // ── Step 0: rewrite the shared AgentCore template ─────────
        // Clone-on-write: if other sessions still hold `Arc<AgentCore>`
        // clones (they do - see `SessionTask::new`'s `(*core).clone()`),
        // the template is deep-cloned before mutation. That's expensive
        // but correct: it isolates the template from in-flight sessions,
        // each of which carries its own `core_mut` snapshot anyway. The
        // cost is bounded - `RuntimeConfigUpdate` is a rare event (user
        // toggles a setting in the Settings panel).
        //
        // For `tool_compression_enabled` this also sync-gates the
        // platform tools (`sync_platform_tools_to_registry`) and rebuilds
        // `all_tools` on the template, so the LLM-visible spec list of
        // every session opened later reflects the toggle.
        Arc::make_mut(&mut self.core).apply_runtime_config(overrides);

        // ── Step 1: broadcast to SessionTask inboxes (for tool definitions etc.) ──
        // The handler rebuilds `ContextBuilder.tool_definitions` so the
        // LLM sees the new set on the next `build_chat_request`.
        let sessions = self.broadcast(SessionMessage::UpdateRuntimeConfig(overrides.clone()));

        // ── Step 2: deliver via send_inbound() fast channel ──
        // Mid-execution AgentLoops pick up the change immediately (the
        // SessionMessage above queues until the next idle boundary).
        let user_op = UserOp::UpdateRuntimeConfig(overrides.clone());
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

        // ADR-039: temperature no longer mirrors into the runtime snapshot.
        // The override propagates through UpdateRuntimeConfig → AgentLoop →
        // SessionState.set_temperature → meta_change_tx → meta.json +
        // session_meta MQTT channel. The Gateway pull API reads from
        // data/meta/{session_id}.json so the new value is reflected on the
        // next fetchSessionState (or immediately via the live MQTT message).

        // ADR-047 3.3.3: temperature is a config field that must be
        // persisted to meta.json + trigger MQTT config notification.
        // The runtime override path above only sets the transient
        // AgentCore.temperature_override (not persisted). Here we also
        // route temperature through apply_config() so it lands in
        // ConversationSession (persisted) + config_version increment +
        // MQTT session/config retained message.
        if let Some(temp) = overrides.temperature {
            for (session_id, handle) in &self.sessions {
                if let Some(ref conv) = handle.conversation {
                    let delta = crate::agent::session_config::SessionConfigDelta {
                        temperature: Some(temp),
                        ..Default::default()
                    };
                    conv.apply_config(&delta);
                    tracing::info!(
                        session_id = %session_id,
                        temperature = temp,
                        "ADR-047: temperature persisted via apply_config (UpdateRuntimeConfig split)"
                    );
                }
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

    /// ADR-029 + ADR-052: Apply builtin-tools enabled flags from a Gateway
    /// `RuntimeConfigUpdate` agent-wide.
    ///
    /// Companion of [`Self::apply_runtime_config_override`] for the
    /// `agent_tools.json` toggle panel - same one-pipeline principle:
    ///   0. Rewrite the shared `AgentCore` **template** enabled flags
    ///      via [`crate::agent::agent_core::AgentCore::apply_builtin_enabled_entries`]
    ///      (clone-on-write: in-flight sessions that already deep-cloned
    ///      the template keep their own snapshot - isolated).
    ///   1. Broadcast `SessionMessage::UpdateBuiltinTools` to all active
    ///      sessions; each SessionTask rewrites its own `builtin_tools`
    ///      enabled flags via the **same** shared policy helper, then
    ///      rebuilds dispatch list + LLM `tool_definitions` atomically
    ///      (`session_task::apply_builtin_tools_update`).
    ///
    /// Without step 0, a session opened AFTER this call would deep-clone
    /// the template's stale enabled flags - the same template-drift
    /// bug `apply_runtime_config_override` was created to fix for
    /// `RuntimeConfigUpdate`. Step 1 is what makes the LLM-visible
    /// `ContextBuilder.tool_definitions` of every active session pick
    /// up the change in lock-step.
    ///
    /// Persistence (`agent_tools.json`) is the HTTP-layer UseCase's
    /// job; this function only mutates in-memory state. Per-session
    /// broadcast failures (closed session, full channel) are logged
    /// but do NOT fail the call - the template update already
    /// succeeded, and closed sessions will pick up the new state via
    /// the on-disk file on their next open.
    pub fn apply_builtin_tools_enabled(
        &mut self,
        entries: &[crate::agent_config::AgentToolEntry],
    ) {
        tracing::info!(
            entry_count = entries.len(),
            enabled_count = entries.iter().filter(|e| e.enabled).count(),
            "SessionManager: applying builtin tools enabled list (template sync + broadcast)"
        );
        Arc::make_mut(&mut self.core).apply_builtin_enabled_entries(entries);
        let failed = self.broadcast(SessionMessage::UpdateBuiltinTools {
            entries: entries.to_vec(),
        });
        if !failed.is_empty() {
            tracing::warn!(
                failed_sessions = failed.len(),
                "apply_builtin_tools_enabled: some sessions missed the broadcast (likely closed)"
            );
        }
    }

    // ── ADR-030 C3: dynamic builtin tool registration (SidecarEndpointUpdate) ──
    //
    // When a sidecar's state changes (LSP relay became ready, sidecar
    // went away, ...), `cli.rs` calls these methods. They wrap the raw
    // tool with the same security decorators used at startup (path guard
    // + rate limiter) and broadcast a `SessionMessage` so every active
    // session rebuilds its dispatch list.

    /// Register a dynamic builtin tool. The raw `Arc<dyn Tool>` is
    /// wrapped with `PathGuardedTool` and `RateLimitedTool` (matching
    /// the startup path in `ToolRegistry::activate`) and then broadcast
    /// to all sessions. Sessions replace any existing entry with the
    /// same `name()` so a sidecar endpoint re-push is idempotent.
    ///
    /// Caller passes the per-session rate limit (`max_calls_per_minute`)
    /// and the shared `SharedResolver`; both are already available at
    /// the runtime call site (`cli.rs`).
    pub fn register_dynamic_tool(
        &mut self,
        tool: Arc<dyn Tool>,
        resolver: crate::tools::workspace_resolver::SharedResolver,
        max_calls_per_minute: u32,
        enabled: bool,
    ) {
        let name = tool.name().to_string();
        let wrapped = crate::tools::wrappers::wrap_with_security_decorators(
            tool,
            resolver,
            max_calls_per_minute,
        );
        let entry = crate::agent::agent_core::BuiltinToolEntry {
            tool: wrapped,
            enabled,
        };

        // Store on the manager so new sessions created after this push
        // also inherit the tool (ADR-030 review ISSUE-1 fix).
        if let Some(existing) = self
            .dynamic_builtin_tools
            .iter()
            .position(|e| e.name() == name)
        {
            self.dynamic_builtin_tools[existing] = entry.clone();
        } else {
            self.dynamic_builtin_tools.push(entry.clone());
        }

        let failed = self.broadcast(SessionMessage::AddDynamicBuiltinTool { entry });
        tracing::info!(
            tool = %name,
            enabled,
            failed_count = failed.len(),
            "SessionManager: dynamic builtin tool registered"
        );
    }

    /// Unregister a dynamic builtin tool by name. Idempotent: removing a
    /// tool that no session has is still a no-op broadcast (sessions
    /// silently skip unknown names).
    pub fn unregister_dynamic_tool(&mut self, name: &str) {
        // Remove from the manager's stored list so new sessions don't
        // inherit a stale tool (ADR-030 review ISSUE-1 fix).
        self.dynamic_builtin_tools.retain(|e| e.name() != name);

        let failed = self.broadcast(SessionMessage::RemoveDynamicBuiltinTool {
            name: name.to_string(),
        });
        tracing::info!(
            tool = %name,
            failed_count = failed.len(),
            "SessionManager: dynamic builtin tool unregistered"
        );
    }

    /// Returns `(name, enabled)` pairs for all dynamically registered
    /// builtin tools (via `SidecarEndpointUpdate`).
    ///
    /// Used by the `ConfigSnapshot` builder to merge with the persisted
    /// `agent_tools.json` so the frontend tool panel reflects tools that
    /// were registered after startup (e.g. `codebase` when LSP Relay
    /// becomes ready).
    pub fn dynamic_builtin_tool_names(&self) -> Vec<(String, bool)> {
        self.dynamic_builtin_tools
            .iter()
            .map(|e| (e.name(), e.enabled))
            .collect()
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
    /// ADR-047: config persistence is now synchronous via `apply_config()`,
    /// bypassing the serial inference queue. LLM-side effects (provider
    /// rebuild, context builder update) are deferred to the next turn
    /// boundary via version polling in SessionTask.
    ///
    /// The new model's default `reasoning_effort` is resolved here and
    /// pre-set on the `ConversationSession` **before** `apply_config()`
    /// so that the single `notify_config_change()` inside `apply_config`
    /// publishes a snapshot with the correct `reasoning_effort`.
    /// Previously, `apply_config` published a stale value from the
    /// previous model, and `apply_llm_effects` (deferred to the next
    /// turn boundary) cleared it too late - the frontend received the
    /// stale value and its preserve-on-null rule prevented the
    /// subsequent clear signal from taking effect.
    pub fn route_model_switch(
        &self,
        session_id: &str,
        model: String,
        provider: Option<String>,
    ) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            model = %model,
            provider = ?provider,
            "SessionManager: routing model_switch (ADR-047: synchronous apply_config)"
        );
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;
        if let Some(ref conv) = handle.conversation {
            // Resolve the new model's default reasoning_effort using the
            // same three-level priority chain as session init and HTTP GET:
            //   1. persisted (None on model switch - clears any user override)
            //   2. caps.default_reasoning_effort
            //   3. supports_reasoning -> Auto
            //   4. None (model doesn't support reasoning)
            let caps = self.core.get_model_capabilities(&model);
            let default_effort =
                crate::agent::session_config::llm_effects::resolve_effective_reasoning_effort(
                    caps.as_ref(),
                    None, // model switch: no persisted override
                );
            let effort_str = default_effort.as_ref().map(|e| e.to_string());

            // Pre-set reasoning_effort without notify_config_change.
            // apply_config's notify_config_change will publish the
            // correct combined state (new model + new reasoning_effort)
            // in a single MQTT message.
            conv.set_reasoning_effort_raw(effort_str);

            let delta = crate::agent::session_config::SessionConfigDelta {
                model: Some(model),
                provider,
                ..Default::default()
            };
            conv.apply_config(&delta);
        }
        Ok(())
    }

    /// Route per-session reasoning effort override to the target session.
    ///
    /// ADR-047: config persistence is now synchronous via `apply_config()`,
    /// bypassing the serial inference queue. LLM-side effects are deferred
    /// to the next turn boundary.
    pub fn route_reasoning_effort(
        &self,
        session_id: &str,
        effort: String,
    ) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            effort = %effort,
            "SessionManager: routing reasoning_effort (ADR-047: synchronous apply_config)"
        );
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;
        if let Some(ref conv) = handle.conversation {
            let delta = crate::agent::session_config::SessionConfigDelta {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            conv.apply_config(&delta);
        }
        Ok(())
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

        // Update the shared search key vault so backends can resolve API keys.
        {
            let mut vault = self.core.search_key_vault.write().unwrap();
            vault.clear();
            for entry in &search_key_vault {
                vault.insert(entry.provider_id.clone(), entry.api_key.clone());
            }
        }

        // Update the shared search provider list.
        {
            let mut list = self.core.search_provider_list.write().unwrap();
            *list = search_list;
        }
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

    /// Handle embedding config update from Gateway (via SidecarEndpointUpdate(Embed)).
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
            "SessionManager: received embedding config update via SidecarEndpointUpdate"
        );

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

    /// Clear the embedding provider on all sessions (embed sidecar went down).
    ///
    /// Broadcasts `DisableEmbedConfig` so each session clears its ONNX
    /// embedding provider. New sessions created after this call will
    /// inherit the cleared state from the shared template (ADR-030 ISSUE-2).
    pub fn clear_embedding_config(&self) {
        tracing::info!(
            "SessionManager: clearing embedding provider on all sessions (embed sidecar unavailable)"
        );
        let failed = self.broadcast(SessionMessage::DisableEmbedConfig);
        if !failed.is_empty() {
            tracing::warn!(
                failed_count = failed.len(),
                "Some sessions failed to receive DisableEmbedConfig"
            );
        }
    }

    /// Get all active session IDs.
    pub fn active_sessions(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Store the latest session info determined during the startup scan.
    ///
    /// Called from `session_init` after the background session scan completes.
    /// `title` may be `None` if the session has no title yet.
    pub fn set_latest_session(&self, session_id: String, title: Option<String>) {
        *self.latest_session.write().unwrap() = Some((session_id, title));
    }

    /// Get the latest session ID and title (by `last_active_at` descending).
    ///
    /// Returns `None` if the startup scan has not completed yet or if no
    /// sessions exist on disk.
    pub fn latest_session(&self) -> Option<(String, Option<String>)> {
        self.latest_session.read().unwrap().clone()
    }

    /// Look up a session handle by ID.
    pub fn get_session(&self, session_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(session_id)
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get the session runtime snapshot for a specific session.
    ///
    /// ADR-039: persisted fields (model, provider, workspace_id,
    /// reasoning_effort, temperature) are no longer duplicated here — see
    /// `data/meta/{session_id}.json` and the `session_meta` MQTT channel.
    /// Returns `None` only if the session is not found.
    pub fn snapshot_session_state(
        &self,
        session_id: &str,
    ) -> Option<crate::agent::session_state::SessionRuntimeSnapshot> {
        self.sessions
            .get(session_id)
            .map(|handle| handle.snapshot())
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

    /// ADR-028: access the shared [`AgentCore`] so callers (e.g. the
    /// `list_sessions` HTTP handler) can update its agent-scoped token
    /// counters after each on-disk session scan. Returning the underlying
    /// `Arc` keeps the API surface minimal — callers may `merge_token_totals`
    /// or `agent_token_totals()` without taking an extra lock.
    pub fn core(&self) -> Arc<AgentCore> {
        self.core.clone()
    }

    /// ADR-021: Access the shared StreamingStateMap for incremental poll reads.
    ///
    /// Returns a reference to the `Arc<RwLock<HashMap<SessionId, StreamingLine>>>`
    /// so the CLI HTTP handler can call `read_messages_since()`.
    pub fn streaming_lines(&self) -> crate::conversation::StreamingStateMap {
        self.streaming_lines.clone()
    }

    /// ADR-047: Shared session config map for `SessionConfigService`.
    pub fn session_configs(&self) -> crate::usecases::SharedSessionConfigs {
        self.session_configs.clone()
    }

    /// ADR-047: Shared workspace resolver (for SessionConfigService).
    pub fn resolver(&self) -> Option<Arc<std::sync::RwLock<WorkspaceResolver>>> {
        self.resolver.clone()
    }

    /// ADR-022: Per-session committed line count — updated by writer thread
    /// after each disk write. Returns 0 if the session has no conversation
    /// (no writer thread, e.g. ephemeral test sessions).
    ///
    /// Use `read_messages_since`'s fallback (file scan) when this returns 0.
    pub fn committed_lines_for(&self, session_id: &str) -> usize {
        self.session_committed_lines
            .get(session_id)
            .map(|arc| arc.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// ADR-025: Get the per-session delivery cursor.
    ///
    /// If no cursor exists for this session (first incremental poll after
    /// session creation/resume), returns a cursor initialized to
    /// `{committed_lines, 0}` — meaning all existing complete lines are
    /// considered "already delivered" (they were fetched via the initial
    /// full-load request, not via incremental polling).
    pub fn get_delivery_cursor(&self, session_id: &str) -> crate::conversation::DeliveryCursor {
        {
            let cursors = self.session_delivery_cursors.read().unwrap();
            if let Some(&c) = cursors.get(session_id) {
                return c;
            }
        }
        // Initialize: all existing committed lines are "delivered".
        let initial = crate::conversation::DeliveryCursor {
            line_number: self.committed_lines_for(session_id),
            char_offset: 0,
        };
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.entry(session_id.to_string()).or_insert(initial);
        initial
    }

    /// ADR-025: Advance the delivery cursor after a successful incremental poll.
    pub fn advance_delivery_cursor(
        &self,
        session_id: &str,
        line_number: usize,
        char_offset: usize,
    ) {
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.insert(
            session_id.to_string(),
            crate::conversation::DeliveryCursor { line_number, char_offset },
        );
    }

    /// ADR-025: Reset the delivery cursor to `total_lines`.
    ///
    /// Called after a non-incremental (full-load) request — all existing
    /// lines have been delivered via pagination, so the cursor jumps to
    /// the current end of the file.
    pub fn reset_delivery_cursor(&self, session_id: &str, total_lines: usize) {
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.insert(
            session_id.to_string(),
            crate::conversation::DeliveryCursor {
                line_number: total_lines,
                char_offset: 0,
            },
        );
    }

    /// ADR-022: Create a fresh `committed_lines` Arc for a new session's
    /// writer thread. The Arc is cloned and stored in
    /// `session_committed_lines` by `create_session_with_id_and_conversation`.
    pub fn new_committed_lines() -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::new(std::sync::atomic::AtomicUsize::new(0))
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
                let snap = handle.snapshot.read().ok()?;
                let model = snap.model.clone()?;
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
                let snap = handle.snapshot.read().ok()?;
                let model = snap.model.clone()?;
                let provider = snap.provider.clone()?;
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
            if let Some(ref snapshots) = self.config.session_snapshots {
                snapshots.write().unwrap().remove(&id);
            }
            self.session_configs.write().unwrap().remove(&id);
            self.session_committed_lines.remove(&id);
            self.session_delivery_cursors.write().unwrap().remove(&id);
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
    /// file on disk. The session can be re-activated later via lazy
    /// resume: the frontend must explicitly send the `open_session`
    /// MQTT command (ADR-038), which routes through
    /// `gateway_loop::handle_open_session` → `SessionManager::open`.
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
                if let Some(ref snapshots) = self.config.session_snapshots {
                    snapshots.write().unwrap().remove(session_id);
                }
                self.session_configs.write().unwrap().remove(session_id);
                self.urgent_stops.remove(session_id);
                self.session_committed_lines.remove(session_id);
                self.session_delivery_cursors.write().unwrap().remove(session_id);
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
    /// Synchronously updates the session's workspace ID and resolved work_dir
    /// on the shared [`SessionCore`] Arc — no channel delay. Also persists the
    /// workspace_id to JSONL via async message.
    ///
    /// When `resolver` is available, resolves the workspace_id to a filesystem
    /// path and writes it to `current_work_dir` synchronously.
    pub fn set_session_workspace(&mut self, session_id: &str, workspace_id: &str) {
        // Remove from pending if the workspace is now active
        self.pending_workspaces.remove(session_id);
        tracing::info!(
            session_id = %session_id,
            workspace_id = %workspace_id,
            "SessionManager: session workspace updated (synchronous)"
        );

        if let Some(handle) = self.sessions.get(session_id) {
            // Write workspace_id synchronously — emit_session_state and
            // list_sessions will see the new value immediately.
            *handle.workspace_id.write().unwrap() = workspace_id.to_string();

            // Resolve and write current_work_dir synchronously
            if let Some(ref resolver) = self.resolver {
                let guard = resolver.read().unwrap();
                let resolved_path = if workspace_id == "__agent_home__" {
                    guard.agent_home().to_string()
                } else {
                    guard
                        .find_by_id(workspace_id)
                        .map(|d| d.path.clone())
                        .unwrap_or_else(|| guard.agent_home().to_string())
                };
                *handle.current_work_dir.write().unwrap() = Some(resolved_path);
            }

            // ADR-047: persist workspace_id to meta.json + notify MQTT
            // synchronously via apply_config(). No longer goes through
            // the serial inference queue.
            if let Some(ref conv) = handle.conversation {
                let delta = crate::agent::session_config::SessionConfigDelta {
                    workspace_id: Some(workspace_id.to_string()),
                    ..Default::default()
                };
                conv.apply_config(&delta);
            }
        }
    }

    /// Set the session workspace.
    ///
    /// Convenience alias for callers that already hold a resolver guard.
    /// Delegates to [`set_session_workspace`] which handles resolver
    /// resolution internally.
    pub fn set_session_workspace_with_resolver(
        &mut self,
        session_id: &str,
        workspace_id: &str,
    ) {
        self.set_session_workspace(session_id, workspace_id);
    }

    /// ADR-034 §8 Phase 2-3: consolidated workspace switch entry point.
    ///
    /// Single source of truth — every workspace switch (gRPC era's
    /// `workspace_switch`, MQTT's `WorkspaceSwitch` command, future
    /// internal callers) MUST go through this method instead of calling
    /// `set_session_workspace` / `update_session_workspace_context`
    /// directly.
    ///
    /// Combines 4 steps from the legacy gRPC-era `route_workspace_switch`
    /// (§6 P0-C / P0-D fix):
    ///   1. Validate `workspace_id` against `allowed_dirs` (resolver)
    ///   2. If invalid: register as `pending_workspace` + fall back to
    ///      `"__agent_home__"` (ADR-034 §4.3 step 4 — re-addable later)
    ///   3. Synchronously set the session's workspace_id + current_work_dir
    ///      via `set_session_workspace`
    ///   4. Push the per-session workspace context + prompt file via
    ///      `update_session_workspace_context`
    ///
    /// Original gRPC-era implementation lives in `cli.rs` (deprecated,
    /// to be removed in Phase 4).
    pub fn route_workspace_switch(&mut self, session_id: &str, workspace_id: &str) {
        // Step 1: validate against the resolver's allowed_dirs.
        let is_valid = if workspace_id == "__agent_home__" {
            true
        } else {
            match self.resolver.as_ref() {
                Some(resolver) => {
                    let guard = resolver.read().unwrap();
                    guard.find_by_id(workspace_id).is_some()
                }
                None => {
                    tracing::warn!(
                        session_id = %session_id,
                        workspace_id = %workspace_id,
                        "route_workspace_switch: resolver not set — accepting workspace_id without validation"
                    );
                    true
                }
            }
        };

        let effective_workspace_id = if is_valid {
            workspace_id.to_string()
        } else {
            // Step 2: invalid → register as pending + fallback to agent home.
            tracing::warn!(
                session_id = %session_id,
                workspace_id = %workspace_id,
                "route_workspace_switch: workspace_id not in allowed_dirs — registered as pending, fallback to __agent_home__"
            );
            self.add_pending_workspace(session_id, workspace_id);
            "__agent_home__".to_string()
        };

        // Step 3: synchronously set workspace_id + current_work_dir + JSONL persist.
        self.set_session_workspace(session_id, &effective_workspace_id);

        // Step 4: push per-session workspace context + prompt file to the SessionTask.
        self.update_session_workspace_context(session_id);

        tracing::info!(
            session_id = %session_id,
            requested = %workspace_id,
            effective = %effective_workspace_id,
            "route_workspace_switch: complete"
        );
    }

    /// Get the current workspace ID for a session.
    /// Returns `"__agent_home__"` if the session has no explicit workspace set
    /// or the session is not found.
    pub fn session_workspace_id(&self, session_id: &str) -> String {
        self.sessions
            .get(session_id)
            .map(|h| h.workspace_id.read().unwrap().clone())
            .unwrap_or_else(|| "__agent_home__".to_string())
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
        let context_text = format_workspace_context_for_session(&resolver_guard, &ws_id);
        let prompt_file_content = resolver_guard.read_prompt_file(&ws_id);
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
        // Collect sessions whose workspace was deleted
        for (sid, handle) in &self.sessions {
            let ws_id = handle.workspace_id.read().unwrap().clone();
            if ws_id == "__agent_home__" {
                continue;
            }
            if resolver.find_by_id(&ws_id).is_none() {
                changes.push((sid.clone(), ws_id));
            }
        }
        for (sid, old_ws_id) in changes {
            self.pending_workspaces
                .insert(sid.clone(), old_ws_id.clone());
            if let Some(handle) = self.sessions.get(&sid) {
                *handle.workspace_id.write().unwrap() = "__agent_home__".to_string();
                *handle.current_work_dir.write().unwrap() = Some(resolver.agent_home().to_string());
                // ADR-047: persist the fallback to meta.json synchronously
                // via apply_config() so cold restarts don't re-read the
                // deleted workspace_id from metadata.
                if let Some(ref conv) = handle.conversation {
                    let delta = crate::agent::session_config::SessionConfigDelta {
                        workspace_id: Some("__agent_home__".to_string()),
                        ..Default::default()
                    };
                    conv.apply_config(&delta);
                }
            }
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

    /// Register a pending workspace for a session (when the workspace
    /// doesn't exist in the resolver yet, but may be re-added later).
    pub fn add_pending_workspace(&mut self, session_id: &str, workspace_id: &str) {
        self.pending_workspaces
            .insert(session_id.to_string(), workspace_id.to_string());
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
                    control_notify: guard.control_notify_handle(),
                }
            } else {
                // No sessions exist yet — create a minimal controller just for
                // its notify handles. Its iteration/phase state will never be read.
                let ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
                let ctrl_for_lock = ctrl.clone();
                let (rw, rs, rc) = {
                    let guard = ctrl_for_lock.lock().await;
                    (
                        guard.rewind_notify_handle(),
                        guard.resume_notify_handle(),
                        guard.control_notify_handle(),
                    )
                };
                DebugHandles {
                    debug_ctrl: ctrl,
                    debug_event_tx: debug_event_tx.clone(),
                    rewind_notify: rw,
                    resume_notify: rs,
                    control_notify: rc,
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
            let (per_rewind, per_resume, per_control) = {
                let guard = per_session_ctrl.lock().await;
                (
                    guard.rewind_notify_handle(),
                    guard.resume_notify_handle(),
                    guard.control_notify_handle(),
                )
            };
            let per_session_handles = DebugHandles {
                debug_ctrl: per_session_ctrl,
                debug_event_tx: handles.debug_event_tx.for_session(sid.clone()),
                rewind_notify: per_rewind,
                resume_notify: per_resume,
                control_notify: per_control,
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
        ov.merge(&RuntimeConfigOverrides {
            max_output_tokens: Some(100),
            ..Default::default()
        });
        assert!(!ov.is_empty());
        assert_eq!(ov.max_output_tokens, Some(100));

        // Re-merge with Some replaces
        ov.merge(&RuntimeConfigOverrides {
            max_output_tokens: Some(200),
            ..Default::default()
        });
        assert_eq!(ov.max_output_tokens, Some(200));

        // None preserves
        ov.merge(&RuntimeConfigOverrides::default());
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

    // ── ADR-052 hot-reload regression: new-session sees toggled tools ──
    //
    // Bug history: prior to this fix,
    // `SessionManagerConfig.tool_definitions` was a frozen snapshot
    // populated at boot. When the Gateway pushed
    // `tool_compression_enabled=false`, the active sessions updated
    // (live `UpdateRuntimeConfig` broadcast → SessionTask handler →
    // `AgentCore.apply_runtime_config` →
    // `sync_platform_tools_to_registry`), but the snapshot used to
    // seed NEW sessions stayed stale. Sessions created after the
    // toggle would advertise `context_retrieve` / `context_abandon`
    // to the LLM even though the dispatch list could no longer run
    // them.
    //
    // Fix architecture:
    // 1. `SessionManagerConfig.tool_definitions` field removed
    //    entirely. There is no longer any pre-baked spec list that
    //    can drift from `core.builtin_tools`.
    // 2. `SessionTask::new` no longer takes a `tool_definitions`
    //    argument. It deep-clones `core.builtin_tools` and applies
    //    `runtime_overrides` (which `SessionManager` accumulates from
    //    every Gateway push). The freshly-built per-session core
    //    therefore reflects the latest `tool_compression_enabled`
    //    value before the session's first LLM call.
    // 3. `SessionManager.apply_runtime_config_override` ALSO
    //    synchronously calls `Arc::make_mut(&mut self.core)
    //    .apply_runtime_config(overrides)` so the template itself
    //    stays current — useful for any code paths that read
    //    `self.core.all_tools` directly without going through
    //    `SessionTask::new`.
    // 4. `SessionTask::run` rebuilds the initial
    //    `ContextBuilder.tool_definitions` from the freshly-built
    //    `core.builtin_tools` via the existing
    //    `rebuild_context_tool_definitions` helper (single source of
    //    derivation logic, same as hot-reload path).
    //
    // This test pins the fix end-to-end at the LLM-visible layer: it
    // mirrors the boot → toggle → new-session lifecycle and asserts
    // that a session created AFTER the toggle does not see platform
    // tools in its `ContextBuilder`. Pre-fix this would fail.
    #[tokio::test]
    async fn test_new_session_after_compression_toggle_omits_platform_tools() {
        use crate::agent::agent_core::BuiltinToolEntry;
        use crate::agent::session::session_task::SessionTask;

        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.snap"
            version = "1.0.0"
            name = "Test Snap"
            description = "Snapshot regression test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(
            acowork_core::providers::mock::MockProvider::single_text("test"),
        );

        // Build a placeholder AgentCore to obtain queues, then
        // discard it and build the real one with platform tools
        // already attached. This mirrors boot-time
        // `tool_compression_enabled=true`.
        let probe = Arc::new(AgentCore::new(
            config.clone(),
            manifest.clone(),
            provider.clone(),
            Vec::<BuiltinToolEntry>::new(),
        ));
        let platform_tools = crate::tools::builtin::build_platform_protected_tools(
            "/tmp",
            probe.retrieve_queue.clone(),
            probe.abandon_queue.clone(),
        );
        let mut initial_builtins: Vec<BuiltinToolEntry> = Vec::new();
        for tool in platform_tools {
            initial_builtins.push(BuiltinToolEntry::with_resolved_enabled(false, tool));
        }
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            initial_builtins,
        ));

        // Sanity: at boot with `tool_compression_enabled=true`, the
        // template's `builtin_tools` list contains the platform
        // tools.
        let boot_names: Vec<String> = core
            .builtin_tools
            .iter()
            .map(|e| e.tool.name())
            .collect();
        assert!(
            boot_names.iter().any(|n| n == "context_retrieve"),
            "boot template must include context_retrieve when compression enabled; got: {:?}",
            boot_names
        );

        let mut manager = SessionManager::new(core.clone(), SessionManagerConfig::default());

        // ── Gateway pushes `tool_compression_enabled=false` ────────
        manager.apply_runtime_config_override(&RuntimeConfigOverrides {
            tool_compression_enabled: Some(false),
            ..Default::default()
        });

        // ── User opens a fresh session AFTER the toggle ───────────
        // Build the per-session AgentCore the same way `SessionTask::new`
        // does (deep-clone the live template — `manager.core`, which has
        // already been hot-reloaded — + apply accumulated runtime
        // overrides + rebuild ContextBuilder tool definitions from
        // the per-session builtin_tools list).
        let mut session_core = (*manager.core).clone();
        let overrides = manager.runtime_overrides.clone();
        session_core.apply_runtime_config(&overrides);

        // Apply dynamic / MCP injections the same way SessionTask::new does.
        session_core.mcp_tools = manager.mcp_tools.clone();
        for entry in &manager.dynamic_builtin_tools {
            let name = entry.name();
            if let Some(existing) = session_core
                .builtin_tools
                .iter()
                .position(|e| e.name() == name)
            {
                session_core.builtin_tools[existing] = entry.clone();
            } else {
                session_core.builtin_tools.push(entry.clone());
            }
        }
        session_core.rebuild_all_tools();

        // Verify the post-toggle template no longer carries the
        // platform tools. This is what gets propagated into the
        // per-session clone via deep-clone of `builtin_tools`.
        //
        // NB: we assert against `manager.core` (the live template),
        // not the `core` local. The local is a separate `Arc`
        // reference; under `Arc::make_mut` clone-on-write semantics,
        // the template may have been deep-cloned into a fresh
        // `AgentCore` once `apply_runtime_config_override` mutated
        // it. The original `core` Arc still points at the pre-toggle
        // snapshot. The `manager.core` reference is what subsequent
        // `SessionTask::new` calls would actually clone from.
        let template_names_after: Vec<String> = manager
            .core
            .builtin_tools
            .iter()
            .map(|e| e.tool.name())
            .collect();
        assert!(
            !template_names_after.iter().any(|n| n == "context_retrieve"),
            "template builtin_tools must drop context_retrieve after toggle; got: {:?}",
            template_names_after
        );
        assert!(
            !template_names_after.iter().any(|n| n == "context_abandon"),
            "template builtin_tools must drop context_abandon after toggle; got: {:?}",
            template_names_after
        );

        // Verify the per-session core (what `SessionTask::new` would
        // construct) also drops them.
        let session_names: Vec<String> = session_core
            .builtin_tools
            .iter()
            .map(|e| e.tool.name())
            .collect();
        assert!(
            !session_names.iter().any(|n| n == "context_retrieve"),
            "new-session builtin_tools must drop context_retrieve after toggle; got: {:?}",
            session_names
        );
        assert!(
            !session_names.iter().any(|n| n == "context_abandon"),
            "new-session builtin_tools must drop context_abandon after toggle; got: {:?}",
            session_names
        );

        // Verify the LLM-visible spec list (what `ContextBuilder` sees
        // after `rebuild_context_tool_definitions`) does not contain
        // them either. This is the actual symptom the user reported.
        let mut context_builder = crate::agent::context::ContextBuilder::new(String::new());
        // We can't directly invoke the private
        // `rebuild_context_tool_definitions` from here without
        // exposing it, so replicate the derivation inline. It MUST
        // stay identical to `rebuild_context_tool_definitions` in
        // session_task.rs — see that function's docstring.
        let llm_visible: Vec<serde_json::Value> = session_core
            .builtin_tools
            .iter()
            .filter(|e| e.enabled)
            .map(|e| serde_json::to_value(&e.tool.spec()).unwrap_or_default())
            .collect();
        context_builder.set_tool_definitions(llm_visible);

        let visible_names: Vec<String> = context_builder
            .tool_definitions()
            .map(|t| {
                t.iter()
                    .filter_map(|v| {
                        v.get("name").and_then(|n| n.as_str()).map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !visible_names.iter().any(|n| n == "context_retrieve"),
            "ContextBuilder.tool_definitions for a NEW session must omit context_retrieve after toggle off; got: {:?}",
            visible_names
        );
        assert!(
            !visible_names.iter().any(|n| n == "context_abandon"),
            "ContextBuilder.tool_definitions for a NEW session must omit context_abandon after toggle off; got: {:?}",
            visible_names
        );

        // Make sure SessionTask::new signature no longer requires a
        // tool_definitions argument (compile-time check). The
        // `_task_builder` closure below only references the type —
        // we don't actually run it.
        let _check_signature: fn(
            Arc<AgentCore>,
            crate::agent::session_state::SessionState,
            tokio::sync::mpsc::Receiver<crate::agent::session::session_task::SessionMessage>,
            String,
            Option<tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
            String,
            Option<String>,
            acowork_core::protocol::ProtocolType,
            Option<Vec<Arc<dyn acowork_core::tools::traits::Tool>>>,
            Vec<BuiltinToolEntry>,
            Option<crate::debug::DebugHandles>,
            Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>,
            RuntimeConfigOverrides,
            Arc<std::sync::RwLock<Option<String>>>,
            Arc<std::sync::atomic::AtomicUsize>,
            crate::conversation::StreamingStateMap,
        ) -> (SessionTask, tokio::sync::mpsc::Sender<crate::agent::inbound::InboundMessage>) =
            SessionTask::new;
    }

    // ── ADR-052 §3.5 hot-reload regression: template drift ────────────
    //
    // Bug history (pre-fix): `apply_builtin_tools_enabled` only
    // broadcast `SessionMessage::UpdateBuiltinTools` to active sessions
    // via `send_to_session`. A session opened AFTER the PUT would
    // deep-clone the shared `AgentCore` template's stale enabled flags
    // - the same template-drift pattern that
    // `apply_runtime_config_override`'s step 0 was created to fix for
    // `RuntimeConfigUpdate`.
    //
    // Fix architecture: `apply_builtin_tools_enabled` now ALSO runs
    // `Arc::make_mut(&mut self.core).apply_builtin_enabled_entries(entries)`
    // before broadcasting, so the template the NEXT session will clone
    // from already carries the new flags. This test pins that
    // contract end-to-end.
    #[tokio::test]
    async fn test_builtin_tools_enabled_syncs_template_for_future_sessions() {
        use crate::agent_config::AgentToolEntry;
        use crate::agent::agent_core::BuiltinToolEntry;

        // Boot an AgentCore with platform tools registered (compression
        // = true at boot, mimicking a real production boot).
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.builtin_sync"
            version = "1.0.0"
            name = "Test builtin sync"
            description = "Pin template drift fix"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(
            acowork_core::providers::mock::MockProvider::single_text("test"),
        );
        let probe = Arc::new(AgentCore::new(
            config.clone(),
            manifest.clone(),
            provider.clone(),
            Vec::<BuiltinToolEntry>::new(),
        ));
        let platform_tools = crate::tools::builtin::build_platform_protected_tools(
            "/tmp",
            probe.retrieve_queue.clone(),
            probe.abandon_queue.clone(),
        );
        let mut initial_builtins: Vec<BuiltinToolEntry> = Vec::new();
        for tool in platform_tools {
            initial_builtins.push(BuiltinToolEntry::with_resolved_enabled(false, tool));
        }
        let core = Arc::new(AgentCore::new(config, manifest, provider, initial_builtins));

        let mut manager = SessionManager::new(core.clone(), SessionManagerConfig::default());

        // Pre-condition: `shell` is NOT in the registry yet (only the
        // platform tools were seeded). The PUT patch below will
        // attempt to enable it; the template must gain it.
        assert!(
            manager
                .core
                .builtin_tools
                .iter()
                .find(|e| e.name() == "shell")
                .is_none(),
            "test precondition: shell must not be in the initial template"
        );

        // Hostile (but realistic) PUT payload that BOTH:
        //   (a) tries to disable a platform tool (must be silently
        //       dropped by the shared policy), and
        //   (b) tries to enable a tool that isn't in the code registry
        //       yet (must also be silently dropped - we only rewrite
        //       enabled flags of ALREADY-registered slots).
        // We use `shell` (a real builtin) for the enable assertion.
        let patch = vec![
            AgentToolEntry::new("context_retrieve", false),
            AgentToolEntry::new("shell", true),
        ];

        // Inject a `shell` slot into the template so the policy has
        // something to rewrite.
        struct DummyShell;
        #[async_trait::async_trait]
        impl acowork_core::tools::traits::Tool for DummyShell {
            fn name(&self) -> String { "shell".to_string() }
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: "shell".to_string(),
                    description: "test shell".to_string(),
                    input_schema: serde_json::json!({}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: String::new(),
                    error: None,
                    token_usage: None,
                })
            }
        }
        Arc::make_mut(&mut manager.core)
            .builtin_tools
            .push(BuiltinToolEntry::with_resolved_enabled(
                false,
                Arc::new(DummyShell),
            ));
        Arc::make_mut(&mut manager.core).rebuild_all_tools();

        manager.apply_builtin_tools_enabled(&patch);

        // Pin the new contract: a session opened AFTER the PUT must
        // deep-clone a template that reflects the patch.
        let mut future_session_core = (*manager.core).clone();
        future_session_core.apply_runtime_config(&manager.runtime_overrides);
        future_session_core.rebuild_all_tools();

        let shell_enabled = future_session_core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .expect("future session's template must include shell")
            .enabled;
        assert!(
            shell_enabled,
            "template must reflect the PUT patch for future sessions (the bug we fixed)"
        );

        // Platform tools must STILL be enabled in the template (the
        // hostile disable request was filtered out by the shared
        // policy). This is the bug-2 regression net at the
        // SessionManager level.
        for name in crate::tools::registry::PLATFORM_PROTECTED_TOOLS {
            let entry = future_session_core
                .builtin_tools
                .iter()
                .find(|e| e.name() == *name)
                .unwrap_or_else(|| panic!("{name} must still be in the future-session template"));
            assert!(
                entry.enabled,
                "{name} must stay enabled in future-session template after PUT (Bug 2 regression net)"
            );
        }
    }

    // ── DeliveryCursor storage tests (ADR-025) ──────────────────────
    //
    // These tests verify the cursor storage logic that SessionManager
    // uses internally (RwLock<HashMap<String, DeliveryCursor>>).  The
    // methods get/advance/reset are thin wrappers around HashMap ops,
    // but testing the lifecycle ensures correctness of the integration
    // with read_messages_since_cursor.

    use crate::conversation::DeliveryCursor;

    /// Simulates SessionManager's delivery cursor storage for testing.
    struct CursorStore {
        cursors: std::sync::RwLock<HashMap<String, DeliveryCursor>>,
        committed_lines: HashMap<String, Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl CursorStore {
        fn new() -> Self {
            Self {
                cursors: std::sync::RwLock::new(HashMap::new()),
                committed_lines: HashMap::new(),
            }
        }

        /// Mirrors SessionManager::get_delivery_cursor
        fn get(&self, sid: &str) -> DeliveryCursor {
            {
                let c = self.cursors.read().unwrap();
                if let Some(&v) = c.get(sid) {
                    return v;
                }
            }
            let cl = self.committed_lines.get(sid)
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            let initial = DeliveryCursor { line_number: cl, char_offset: 0 };
            self.cursors.write().unwrap().entry(sid.to_string()).or_insert(initial);
            initial
        }

        /// Mirrors SessionManager::advance_delivery_cursor
        fn advance(&self, sid: &str, line_number: usize, char_offset: usize) {
            self.cursors.write().unwrap().insert(
                sid.to_string(),
                DeliveryCursor { line_number, char_offset },
            );
        }

        /// Mirrors SessionManager::reset_delivery_cursor
        fn reset(&self, sid: &str, total_lines: usize) {
            self.cursors.write().unwrap().insert(
                sid.to_string(),
                DeliveryCursor { line_number: total_lines, char_offset: 0 },
            );
        }

        /// Mirrors session cleanup (close/delete/evict)
        fn remove(&self, sid: &str) {
            self.cursors.write().unwrap().remove(sid);
        }

        fn set_committed_lines(&mut self, sid: &str, count: usize) {
            self.committed_lines.insert(
                sid.to_string(),
                Arc::new(std::sync::atomic::AtomicUsize::new(count)),
            );
        }

        /// Mirrors SessionManager::committed_lines_for
        fn committed_lines_for(&self, sid: &str) -> usize {
            self.committed_lines.get(sid)
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0)
        }
    }

    #[test]
    fn test_cursor_get_initializes_to_committed_lines() {
        let mut store = CursorStore::new();
        store.set_committed_lines("s1", 42);

        // First get — should initialize to {42, 0}
        let c = store.get("s1");
        assert_eq!(c.line_number, 42);
        assert_eq!(c.char_offset, 0);

        // Second get — should return the stored value (not re-initialize)
        store.advance("s1", 50, 10);
        let c = store.get("s1");
        assert_eq!(c.line_number, 50);
        assert_eq!(c.char_offset, 10);
    }

    #[test]
    fn test_cursor_get_no_committed_lines_defaults_to_zero() {
        let store = CursorStore::new();
        // No committed_lines entry for "s2"
        let c = store.get("s2");
        assert_eq!(c.line_number, 0);
        assert_eq!(c.char_offset, 0);
    }

    #[test]
    fn test_cursor_advance_updates_value() {
        let store = CursorStore::new();

        store.advance("s1", 10, 5);
        let c = store.get("s1");
        assert_eq!(c.line_number, 10);
        assert_eq!(c.char_offset, 5);

        // Advance again — overwrites
        store.advance("s1", 15, 20);
        let c = store.get("s1");
        assert_eq!(c.line_number, 15);
        assert_eq!(c.char_offset, 20);
    }

    #[test]
    fn test_cursor_reset_sets_to_total_lines() {
        let store = CursorStore::new();

        // Advance to some position
        store.advance("s1", 5, 10);
        assert_eq!(store.get("s1").line_number, 5);

        // Full load resets to total_lines
        store.reset("s1", 100);
        let c = store.get("s1");
        assert_eq!(c.line_number, 100);
        assert_eq!(c.char_offset, 0);
    }

    #[test]
    fn test_cursor_remove_clears_entry() {
        let store = CursorStore::new();
        store.advance("s1", 10, 5);
        assert!(store.cursors.read().unwrap().contains_key("s1"));

        store.remove("s1");
        assert!(!store.cursors.read().unwrap().contains_key("s1"));

        // After removal, get re-initializes from committed_lines
        let c = store.get("s1");
        assert_eq!(c.line_number, 0); // no committed_lines → 0
    }

    #[test]
    fn test_cursor_isolation_between_sessions() {
        let store = CursorStore::new();

        store.advance("s1", 10, 5);
        store.advance("s2", 20, 15);

        assert_eq!(store.get("s1").line_number, 10);
        assert_eq!(store.get("s2").line_number, 20);

        // Advancing s1 doesn't affect s2
        store.advance("s1", 15, 0);
        assert_eq!(store.get("s2").line_number, 20);

        // Removing s1 doesn't affect s2
        store.remove("s1");
        assert_eq!(store.get("s2").line_number, 20);
    }

    /// E2E: Full delivery flow using CursorStore + read_messages_since_cursor.
    /// This is the closest to the real SessionManager + cli.rs path without
    /// requiring a full AgentCore.
    #[test]
    fn test_e2e_cursor_store_with_read_messages() {
        use crate::conversation::{read_messages_since_cursor, ConversationEntry};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let entries = vec![
            ConversationEntry {
                id: "1".to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
                role: "user".to_string(),
                content: "hello".to_string(),
                metadata: None,
                kind: None,
            },
        ];
        let path = dir.path().join("conversations").join("e2e.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            for e in &entries {
                serde_json::to_writer(&mut f, e).unwrap();
                writeln!(f).unwrap();
            }
        }

        let mut store = CursorStore::new();
        store.set_committed_lines("e2e", 1);
        let sid = "e2e";
        let map: crate::conversation::StreamingStateMap =
            Arc::new(std::sync::RwLock::new(HashMap::new()));

        // Step 1: Full load — reset cursor to total_lines
        store.reset(sid, 1);
        let cursor = store.get(sid);
        assert_eq!(cursor.line_number, 1);

        // Step 2: Incremental poll — nothing new
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, store.committed_lines_for(sid)).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
        store.advance(sid, r.new_cursor.line_number, r.new_cursor.char_offset);

        // Step 3: Write new line (simulates flush + writer incrementing committed_lines)
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            let e = ConversationEntry {
                id: "2".to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
                role: "assistant".to_string(),
                content: "world".to_string(),
                metadata: None,
                kind: None,
            };
            serde_json::to_writer(&mut f, &e).unwrap();
            writeln!(f).unwrap();
        }
        // Simulate writer thread incrementing committed_lines
        store.committed_lines.get(sid).unwrap()
            .store(2, std::sync::atomic::Ordering::Relaxed);

        // Step 4: Incremental poll — delivers new line
        let cursor = store.get(sid);
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, store.committed_lines_for(sid)).unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].content, "world");
        store.advance(sid, r.new_cursor.line_number, r.new_cursor.char_offset);

        // Step 5: Incremental poll — nothing new
        let cursor = store.get(sid);
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, store.committed_lines_for(sid)).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
    }
}
