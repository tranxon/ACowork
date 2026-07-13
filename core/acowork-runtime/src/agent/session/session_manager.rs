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
use crate::config::DEFAULT_TEMPERATURE;
use crate::conversation::ConversationSession;
use crate::debug::controller::DebugController;
use crate::error::{Result, RuntimeError};
use crate::agent_config::AgentConfig;
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
    /// ADR-021: Single chunk sender for control events.
    /// When set, each session's AgentLoop forwards control events here
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
            tool_definitions: Vec::new(),
            full_tool_specs: Vec::new(),
            identity_context: None,
            protocol_type: ProtocolType::default(),
            session_snapshots: None,
            latest_session: None,
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
    /// ADR-032: number of recent tool results kept raw (not compressed) at
    /// every trigger point (event / budget / restore / manual). `None`
    /// means "fall through to `crate::agent::loop_context::DEFAULT_KEEP_RECENT_N`".
    /// `Some(0)` compresses every eligible tool result; `Some(n)` keeps the
    /// last `n` tool messages raw. See `docs/adr/zh/ADR-032-context-recall.md`
    /// core principle #7.
    pub tool_result_keep_recent_n: Option<u32>,

    /// ADR-032 C4b: compression trigger mode ("auto" | "manual").
    /// `None` falls through to `crate::agent::loop_context::DEFAULT_COMPRESSION_MODE`.
    pub tool_result_compression_mode: Option<String>,

    /// ADR-032 C4a: tool-result soft compression threshold in characters.
    /// `None` falls through to `AgentConfig::tool_result_soft_threshold_chars`,
    /// then to `crate::agent::loop_context::DEFAULT_SOFT_THRESHOLD_CHARS`.
    ///
    /// Marked **boot-only**: the field is populated when `agent_config.json`
    /// is loaded (`apply_runtime_config_override`) but is intentionally NOT
    /// routed through `RuntimeConfigUpdate` — recomputing byte-offsets on
    /// already-active sessions would invalidate in-flight conversation
    /// history pointers. See ADR-032 C4a, and the matching
    /// `cli.rs::RuntimeConfigUpdate::is_*_boot_only()` taxonomy.
    pub tool_result_soft_threshold_chars: Option<usize>,
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
            && self.tool_result_keep_recent_n.is_none()
            && self.tool_result_compression_mode.is_none()
            && self.tool_result_soft_threshold_chars.is_none()
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
        if other.tool_result_keep_recent_n.is_some() {
            self.tool_result_keep_recent_n = other.tool_result_keep_recent_n;
        }
        if other.tool_result_compression_mode.is_some() {
            self.tool_result_compression_mode = other.tool_result_compression_mode.clone();
        }
        if other.tool_result_soft_threshold_chars.is_some() {
            self.tool_result_soft_threshold_chars = other.tool_result_soft_threshold_chars;
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
        if let Some(v) = self.tool_result_keep_recent_n {
            cfg.tool_result_keep_recent_n = Some(v);
        }
        if let Some(v) = &self.tool_result_compression_mode {
            cfg.tool_result_compression_mode = Some(v.clone());
        }
        if let Some(v) = self.tool_result_soft_threshold_chars {
            cfg.tool_result_soft_threshold_chars = Some(v);
        }
    }
}

impl From<&AgentConfig> for RuntimeConfigOverrides {
    /// Project the subset of `AgentConfig` fields that participate in the
    /// runtime override chain. Other `AgentConfig` fields (max_sessions,
    /// avatar, etc.) are applied separately via their own code paths.
    ///
    /// Pass-through projection: every field maps 1:1 from `AgentConfig`.
    /// In particular, `tool_result_compression_mode = Some("auto")` is
    /// *not* collapsed to `None` — the on-disk value is the user's
    /// explicit choice, and the runtime `compression_mode()` accessor
    /// already maps every non-`"manual"` variant to `Auto`, so the
    /// behaviour is consistent without touching the data.
    fn from(cfg: &AgentConfig) -> Self {
        Self {
            max_output_tokens: cfg.max_output_tokens,
            max_iterations: cfg.max_iterations,
            temperature: cfg.temperature,
            context_window: cfg.context_window,
            system_prompt_override: cfg.system_prompt_override.clone(),
            shell_approval_threshold: cfg.shell_approval_threshold.clone(),
            approval_timeout_secs: cfg.approval_timeout_secs,
            tool_result_keep_recent_n: cfg.tool_result_keep_recent_n,
            tool_result_compression_mode: cfg.tool_result_compression_mode.clone(),
            tool_result_soft_threshold_chars: cfg.tool_result_soft_threshold_chars,
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
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        // Extract shared Arc before config is moved into self.config.
        let latest_session = config
            .latest_session
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(None)));

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
            session_committed_lines: HashMap::new(),
            session_delivery_cursors: std::sync::RwLock::new(HashMap::new()),
            streaming_lines: Arc::new(std::sync::RwLock::new(HashMap::new())),
            latest_session,
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
        // build_initial_session_state; we just need to set session_id and workspace_id.
        let snapshot_arc = session_state.snapshot.clone();
        {
            let ws_id_str = if initial_workspace == "__agent_home__" {
                None
            } else {
                Some(initial_workspace.clone())
            };
            if let Ok(mut snap) = snapshot_arc.write() {
                snap.session_id = session_id.clone();
                snap.workspace_id = ws_id_str;
            }
        }

        let (mut task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.tool_definitions.clone(),
            self.config.identity_context.clone(),
            self.config.protocol_type.clone(),
            self.mcp_tools.clone(),
            self.dynamic_builtin_tools.clone(),
            per_session_debug,
            pending_debug_handles.clone(),
            self.runtime_overrides.clone(),
            workspace_id.clone(),
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

        let conv = crate::conversation::ConversationSession::new(
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

        // Spawn the session task
        self.create_session_with_id_and_conversation(
            session_id.clone(),
            Some(conv),
            Some(committed_lines),
        )
        .await?;

        // ADR-020: Enable notifications for the newly created session
        // (it starts in the foreground, frontend will switch to it immediately).
        if let Some(handle) = self.sessions.get(&session_id) {
            let _ = handle.inbound_tx.try_send(SessionMessage::EnableNotify);
        }

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

            // ── ADR-032 C5: re-apply in-memory compression ────────────────
            // The JSONL always stores raw tool output — compression state is
            // derived at runtime by rules (core principle #6), not persisted.
            // Restore must re-drive compression so that a resumed session
            // starts with the same look as a continuously-running session.
            //
            // This runs *before* `fit_to_budget_lossless` so that compression
            // (lossless) recovers as much headroom as possible before the
            // lossy trim (which discards entire messages).
            let n = self.core.tool_result_keep_recent_n();
            let soft_threshold = self.core.tool_result_soft_threshold_chars();
            let compressed = session_state
                .history_mut()
                .compress_tool_results(soft_threshold, n as usize);
            if compressed > 0 {
                session_state.history_mut().recalibrate_tokens();
                tracing::info!(
                    compressed,
                    keep_recent_n = n,
                    soft_threshold_chars = soft_threshold,
                    "ADR-032 restore: compressed oversized tool results"
                );
            }

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

        // Populate the shared snapshot with persistent data so the frontend
        // can read it via fetchSessionState() immediately, without waiting
        // for emit_session_state() to run.
        {
            let model_name = session_state.model().map(|s| s.to_string());
            let provider_name = session_state.provider().map(|s| s.to_string());
            let effort_str = session_state.reasoning_effort().map(|e| e.to_string());
            let temp = session_state.temperature();

            // Build context_usage from persisted session tokens (if available).
            let context_usage_json = session_state.conversation().and_then(|conv| {
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
                snap.reasoning_effort = effort_str;
                snap.temperature = temp;
                snap.context_usage_json = context_usage_json;
                // session_id and workspace_id are set by the caller after
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

        // ADR-020: Disable notifications before Close to avoid stale events
        let _ = handle.inbound_tx.try_send(SessionMessage::DisableNotify);

        // Send Close signal; ignore errors (session may have already stopped)
        let _ = handle.inbound_tx.send(SessionMessage::Close).await;

        // Clean up per-session mappings
        self.pending_workspaces.remove(session_id);
        self.urgent_stops.remove(session_id);
        self.session_committed_lines.remove(session_id);
        self.session_delivery_cursors.write().unwrap().remove(session_id);

        tracing::info!(session_id = %session_id, "SessionManager: closed session");
        Ok(())
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
            // ADR-020: Disable notifications before Close
            let _ = handle.inbound_tx.try_send(SessionMessage::DisableNotify);
            let _ = handle.inbound_tx.send(SessionMessage::Close).await;

            // Clean up in-memory mappings
            self.pending_workspaces.remove(session_id);
            self.urgent_stops.remove(session_id);
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

    /// Ensure a session is loaded in memory, resuming it from disk if needed.
    ///
    /// This is the **single entry point** for lazy session recovery. Every
    /// handler that needs to interact with a session by ID should call this
    /// before [`send_to_session`] or any other session-scoped method.
    ///
    /// When the session is already in memory, this returns immediately.
    /// Otherwise, it reads the JSONL file from `work_dir/conversations/`,
    /// creates a new `SessionTask`, and inserts the handle into the active
    /// sessions map — exactly as `activate_session` does on first access.
    ///
    /// Returns `Ok(())` if the session is now in memory (either was already,
    /// or was just resumed). Returns an error if the JSONL file does not
    /// exist or cannot be read (session truly does not exist on disk).
    pub async fn ensure_session_in_memory(
        &mut self,
        session_id: &str,
        work_dir: &Path,
    ) -> Result<()> {
        if self.sessions.contains_key(session_id) {
            return Ok(());
        }

        // Each resumed session gets its own committed_lines counter.
        // The writer thread (inside ConversationSession) increments it;
        // the session's AgentCore reads it via clone_for_session.
        let committed_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conv = crate::conversation::ConversationSession::resume(work_dir, session_id, committed_lines.clone())
            .map_err(|e| {
                RuntimeError::Config(format!(
                    "Session not found on disk: {} ({})",
                    session_id, e
                ))
            })?;

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
            "SessionManager: lazy-resumed session from disk"
        );
        Ok(())
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
        overrides: &RuntimeConfigOverrides,
    ) -> Vec<String> {
        self.runtime_overrides.merge(overrides);
        // ── 1. Broadcast to SessionTask channels (for tool definitions etc.) ──
        let sessions = self.broadcast(SessionMessage::UpdateRuntimeConfig(overrides.clone()));

        // ── 2. Also deliver via send_inbound() fast channel ──
        // This ensures the AgentLoop immediately picks up runtime config
        // changes even while mid-execution (streaming / running tools),
        // when the SessionTask's message loop is blocked on agent_loop.run().
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

        // ── 3. Directly patch snapshot so the HTTP pull API returns
        //    the new temperature immediately, even when the AgentLoop is
        //    mid-streaming (the fast-path UserOp won't be consumed until
        //    the next drain_inbound_queue checkpoint).
        if let Some(temp) = overrides.temperature {
            for (session_id, handle) in &self.sessions {
                if let Ok(mut guard) = handle.snapshot.write() {
                    tracing::debug!(
                        session_id = %session_id,
                        old = ?guard.temperature,
                        new = temp,
                        "apply_runtime_config_override: patching snapshot temperature"
                    );
                    guard.temperature = Some(temp);
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

    /// ADR-029: Apply builtin tools enabled flags from a Gateway
    /// `RuntimeConfigUpdate`. Persists to `agent_tools.json` and
    /// broadcasts to all existing sessions.
    pub fn apply_builtin_tools_enabled(
        &mut self,
        entries: &[crate::agent_config::AgentToolEntry],
    ) {
        tracing::info!(
            entry_count = entries.len(),
            enabled_count = entries.iter().filter(|e| e.enabled).count(),
            "SessionManager: applying builtin tools enabled list"
        );
        self.broadcast(SessionMessage::UpdateBuiltinTools {
            entries: entries.to_vec(),
        });
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

    /// Get the session state snapshot for a specific session.
    ///
    /// Returns the current `SessionStateSnapshot` shared with [`SessionState`].
    /// The snapshot is initialized with persistent data during session creation
    /// and updated at runtime by [`AgentLoop::emit_session_state`].
    /// Returns `None` only if the session is not found.
    pub fn snapshot_session_state(
        &self,
        session_id: &str,
    ) -> Option<crate::agent::session_state::SessionStateSnapshot> {
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

    /// Access the Grafeo memory store from the shared core.
    /// Returns None if the memory store was not initialized.
    pub(crate) fn memory_store(&self) -> Option<&Arc<acowork_grafeo::grafeo::GrafeoStore>> {
        self.core.memory_store()
    }

    /// Return the embedding dimension of the currently configured
    /// [`EmbeddingProvider`](crate::embedding::EmbeddingProvider), or 0 if no
    /// provider is configured. Used by the runtime's `MemoryStatsQuery` handler
    /// so the desktop can detect a stored vs. model dimension mismatch.
    pub(crate) fn embedding_provider_dim(&self) -> u64 {
        self.core
            .embedding_provider
            .as_ref()
            .map(|p| p.dimension() as u64)
            .unwrap_or(0)
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
                if let Some(ref snapshots) = self.config.session_snapshots {
                    snapshots.write().unwrap().remove(session_id);
                }
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

            // Persist to JSONL (async, non-blocking)
            let _ = handle.send(SessionMessage::SetWorkspaceId {
                workspace_id: workspace_id.to_string(),
            });
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
                // Persist the fallback to JSONL so cold restarts don't
                // re-read the deleted workspace_id from metadata.
                let _ = handle.send(SessionMessage::SetWorkspaceId {
                    workspace_id: "__agent_home__".to_string(),
                });
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

    // ── ADR-032 tool_result_keep_recent_n config wiring ────────────────

    #[test]
    fn test_overrides_includes_tool_result_keep_recent_n() {
        // Setting the field flips is_empty from true → false and
        // surfaces the value through is_empty / direct field read.
        let mut ov = RuntimeConfigOverrides::default();
        assert!(ov.is_empty());
        ov.tool_result_keep_recent_n = Some(5);
        assert!(!ov.is_empty());
        assert_eq!(ov.tool_result_keep_recent_n, Some(5));
    }

    #[test]
    fn test_overrides_merge_tool_result_keep_recent_n() {
        // The same merge semantics as the other fields: Some replaces,
        // None preserves the cached value.
        let mut ov = RuntimeConfigOverrides::default();
        ov.merge(&RuntimeConfigOverrides {
            tool_result_keep_recent_n: Some(3),
            ..Default::default()
        });
        assert_eq!(ov.tool_result_keep_recent_n, Some(3));

        // Some replaces
        ov.merge(&RuntimeConfigOverrides {
            tool_result_keep_recent_n: Some(0),
            ..Default::default()
        });
        assert_eq!(ov.tool_result_keep_recent_n, Some(0));

        // None preserves the cached Some(0) (the user explicitly opted
        // into the "compress every eligible tool result" mode).
        ov.merge(&RuntimeConfigOverrides::default());
        assert_eq!(ov.tool_result_keep_recent_n, Some(0));
    }

    #[test]
    fn test_agent_config_projects_tool_result_keep_recent_n() {
        // `From<&AgentConfig>` must surface `tool_result_keep_recent_n`
        // verbatim so a user value in agent_config.json flows through
        // to `session_manager.runtime_overrides` at boot.
        let cfg = crate::agent_config::AgentConfig {
            tool_result_keep_recent_n: Some(7),
            ..Default::default()
        };
        let ov = RuntimeConfigOverrides::from(&cfg);
        assert_eq!(ov.tool_result_keep_recent_n, Some(7));

        // None on AgentConfig → None on overrides (caller falls through
        // to the code default `DEFAULT_KEEP_RECENT_N`).
        let cfg_empty = crate::agent_config::AgentConfig::default();
        let ov_empty = RuntimeConfigOverrides::from(&cfg_empty);
        assert_eq!(ov_empty.tool_result_keep_recent_n, None);
    }

    #[test]
    fn test_overrides_apply_to_writes_compression_mode() {
        // Regression for the live-edit persistence bug: a push carrying
        // `tool_result_compression_mode = Some("manual")` must land in
        // the persisted AgentConfig — previously this field was dropped
        // by the runtime and never reached agent_config.json.
        let mut cfg = crate::agent_config::AgentConfig::default();
        let ov = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };
        ov.apply_to(&mut cfg);
        assert_eq!(cfg.tool_result_compression_mode.as_deref(), Some("manual"));
    }

    #[test]
    fn test_overrides_apply_to_writes_all_some_fields() {
        // apply_to is the single source of truth for runtime-overrides →
        // AgentConfig persistence. Every field whose override is Some
        // must land in the persisted config; None fields must be no-ops
        // (so partial pushes don't clobber disk values).
        let mut cfg = crate::agent_config::AgentConfig::default();
        let ov = RuntimeConfigOverrides {
            max_output_tokens: Some(2048),
            max_iterations: Some(20),
            temperature: Some(0.3),
            context_window: Some(8192),
            system_prompt_override: Some("be terse".into()),
            shell_approval_threshold: Some("low".into()),
            approval_timeout_secs: Some(15),
            tool_result_keep_recent_n: Some(3),
            tool_result_compression_mode: Some("manual".into()),
            tool_result_soft_threshold_chars: Some(4096),
        };
        ov.apply_to(&mut cfg);

        assert_eq!(cfg.max_output_tokens, Some(2048));
        assert_eq!(cfg.max_iterations, Some(20));
        assert_eq!(cfg.temperature, Some(0.3));
        assert_eq!(cfg.context_window, Some(8192));
        assert_eq!(cfg.system_prompt_override.as_deref(), Some("be terse"));
        assert_eq!(cfg.shell_approval_threshold.as_deref(), Some("low"));
        assert_eq!(cfg.approval_timeout_secs, Some(15));
        assert_eq!(cfg.tool_result_keep_recent_n, Some(3));
        assert_eq!(cfg.tool_result_compression_mode.as_deref(), Some("manual"));
        assert_eq!(cfg.tool_result_soft_threshold_chars, Some(4096));
    }

    #[test]
    fn test_overrides_apply_to_none_preserves_disk() {
        // A partial push (override with all-`None`) must not clobber any
        // existing disk value. This guards the read-modify-write loop in
        // cli.rs that loads agent_config.json before applying.
        let mut cfg = crate::agent_config::AgentConfig {
            max_output_tokens: Some(4096),
            tool_result_compression_mode: Some("manual".into()),
            max_sessions: Some(7),
            ..Default::default()
        };
        // Snapshot the fields we expect to be preserved (AgentConfig doesn't
        // derive PartialEq, so compare each field individually).
        let pre_max_output_tokens = cfg.max_output_tokens;
        let pre_tool_result_compression_mode = cfg.tool_result_compression_mode.clone();
        let pre_max_sessions = cfg.max_sessions;

        let ov = RuntimeConfigOverrides::default();
        ov.apply_to(&mut cfg);

        assert_eq!(
            cfg.max_output_tokens, pre_max_output_tokens,
            "max_output_tokens must be preserved on all-None push"
        );
        assert_eq!(
            cfg.tool_result_compression_mode, pre_tool_result_compression_mode,
            "tool_result_compression_mode must be preserved on all-None push"
        );
        assert_eq!(
            cfg.max_sessions, pre_max_sessions,
            "max_sessions (not in overrides) must be preserved on all-None push"
        );
    }

    #[test]
    fn test_overrides_apply_to_partial_push_only_writes_pushed_fields() {
        // Mixed push: only `tool_result_compression_mode` is Some; every
        // other field on disk must survive untouched.
        let mut cfg = crate::agent_config::AgentConfig {
            max_output_tokens: Some(4096),
            tool_result_keep_recent_n: Some(5),
            tool_result_compression_mode: Some("auto".into()),
            ..Default::default()
        };
        let pre_max_output_tokens = cfg.max_output_tokens;
        let pre_tool_result_keep_recent_n = cfg.tool_result_keep_recent_n;

        let ov = RuntimeConfigOverrides {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };
        ov.apply_to(&mut cfg);

        assert_eq!(
            cfg.tool_result_compression_mode.as_deref(),
            Some("manual"),
            "pushed field must be updated"
        );
        assert_eq!(
            cfg.max_output_tokens, pre_max_output_tokens,
            "unpushed field must be preserved"
        );
        assert_eq!(
            cfg.tool_result_keep_recent_n, pre_tool_result_keep_recent_n,
            "unpushed field must be preserved"
        );
    }

    #[test]
    fn test_agent_config_projects_compression_mode() {
        // `From<&AgentConfig>` must surface `tool_result_compression_mode`
        // verbatim so the boot path can hydrate runtime_overrides with
        // whatever mode the user persisted in agent_config.json.
        let cfg = crate::agent_config::AgentConfig {
            tool_result_compression_mode: Some("manual".into()),
            ..Default::default()
        };
        let ov = RuntimeConfigOverrides::from(&cfg);
        assert_eq!(ov.tool_result_compression_mode.as_deref(), Some("manual"));

        // Disk None → override None → caller falls through to the
        // default (currently `DEFAULT_COMPRESSION_MODE` = Auto).
        let cfg_empty = crate::agent_config::AgentConfig::default();
        let ov_empty = RuntimeConfigOverrides::from(&cfg_empty);
        assert_eq!(ov_empty.tool_result_compression_mode, None);
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
