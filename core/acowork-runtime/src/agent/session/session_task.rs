//! SessionTask: independent execution actor for a single session.
//!
//! Each `SessionTask` runs in its own tokio task, processing messages
//! from an inbound channel. It owns an `AgentLoop` instance for the
//! session's lifetime, ensuring per-session isolation of history,
//! budget, and loop detection while sharing provider/tools via Arc.

use std::sync::Arc;

use acowork_core::providers::traits::ChatMessage;
use acowork_core::tools::traits::Tool;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::agent::agent_core::AgentCore;
use crate::agent::context::ContextBuilder;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, SessionChunkEvent};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session_state::SessionState;
use crate::debug::DebugHandles;
use crate::debug::DebugObserverImpl;
use crate::tools::builtin::doc_reader::{self, ExtractOptions, detect_format};

/// Messages that can be sent to a SessionTask.
#[derive(Clone)]
pub enum SessionMessage {
    /// User chat message to process
    ChatMessage {
        content: String,
        message_id: String,
        /// Skill instructions to inject into the system prompt (from command-based skill selection).
        /// When set, the instructions are injected via ContextBuilder rather than prepended to user content.
        skill_instructions: Option<String>,
        /// Optional document references uploaded with this message.
        /// Each entry: { "id", "filename", "abs_path", "format", "size" }
        documents: Option<Vec<serde_json::Value>>,
        /// Optional multimodal content parts (e.g. text + image_url).
        /// When present, the agent loop constructs a ChatMessage::user_multimodal()
        /// instead of ChatMessage::user(), enabling image inputs to flow to the LLM.
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
        /// Files/selections attached by the user from workspace explorer / editor.
        /// Each entry: { abs_path, type ("file"/"selection"), start_line?, end_line? }
        /// The Runtime emits file-path references into the user message so the
        /// LLM can use its own tools to read the content on demand.
        attached_context: Option<Vec<acowork_core::protocol::AttachedContextItem>>,
    },
    /// Continue execution after tool result or iteration pause
    ContinueExecution,
    /// Switch the LLM model at runtime (ADR-012: per-session, carries provider).
    /// When `provider` is set, the SessionTask rebuilds the LLM Provider
    /// instance from `AgentCore.build_provider_for(provider_id)`, using
    /// the global provider list + key vault populated at startup.
    ModelSwitch {
        model: String,
        provider: Option<String>,
    },
    /// Set per-session reasoning effort override (from frontend toggle).
    /// When set, overrides the model's default_reasoning_effort.
    /// Set to "none"/"off" to disable reasoning for this session.
    ReasoningEffort {
        effort: String,
    },
    /// Apply runtime config overrides from Gateway
    UpdateRuntimeConfig {
        max_output_tokens: Option<u64>,
        max_iterations: Option<u32>,
        temperature: Option<f32>,
        system_prompt_override: Option<String>,
        shell_approval_threshold: Option<String>,
    },
    /// Update workspace context text
    UpdateWorkspaceContext { context_text: String },
    /// Update MCP tools on AgentCore (hot-push when MCP servers connect/disconnect).
    /// Refreshes `AgentCore.all_tools` so LLM injection and debug snapshot capture
    /// pick up the latest MCP tool list.
    UpdateMcpTools {
        mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    },
    /// Update the title of the session's conversation
    UpdateSessionTitle { title: String },
    /// Persist the per-session workspace_id to the JSONL conversation file
    SetWorkspaceId { workspace_id: String },
    /// Update the workspace directory path for tool execution.
    /// Carries the fully-resolved absolute path from SessionManager.
    SetWorkDir { path: String },
    /// Update the workspace prompt file content (CLAUDE.md / AGENTS.md).
    /// Content is None when no prompt file is configured.
    SetWorkspacePromptFile { content: Option<String> },
    /// Update identity context from Gateway UserProfileUpdate push
    UpdateIdentityContext { identity_context: Option<String> },
    /// Global provider list was updated (Gateway pushed new model capabilities).
    /// Sessions should emit an updated status to the frontend so the UI
    /// reflects the latest available models and providers.
    ProviderListUpdated,
    /// Stop signal to stop the current agent loop iteration
    Stop { reason: String },
    /// Enable debug mode at runtime (after Gateway pushes EnableDebugMode).
    /// Carries the DebugController, event sender, and notify handles so the
    /// SessionTask can inject them into its AgentCore and start emitting
    /// debug events without a process restart.
    EnableDebugMode(DebugHandles),
    /// Close the session gracefully: trigger distillation and free resources.
    /// JSONL history is preserved (use Delete to also remove the file).
    Close,
    /// Manually trigger context compaction (from user-initiated compact_context WebSocket action).
    CompactContext,
    /// Update the embedding provider at runtime (hot-push from Gateway EmbeddingConfigUpdate).
    /// The session rebuilds its ONNX embedding provider with the new endpoint/model/dimension.
    UpdateEmbedConfig {
        embed_endpoint: String,
        embed_model_id: String,
        embed_dimension: usize,
    },
    /// Inject a system notification into the conversation history.
    /// Used to surface MCP connection failures and other async events
    /// to the LLM context so the Agent can self-heal.
    SystemNotification { content: String },
}

impl std::fmt::Debug for SessionMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionMessage::ChatMessage {
                content,
                message_id,
                skill_instructions,
                documents,
                content_parts,
                attached_context,
            } => f
                .debug_struct("ChatMessage")
                .field("content", &content.chars().take(64).collect::<String>())
                .field("message_id", message_id)
                .field("has_skill", &skill_instructions.is_some())
                .field("has_docs", &documents.is_some())
                .field("has_content_parts", &content_parts.is_some())
                .field(
                    "attached_count",
                    &attached_context.as_ref().map(|c| c.len()).unwrap_or(0),
                )
                .finish(),
            SessionMessage::ContinueExecution => f.debug_tuple("ContinueExecution").finish(),
            SessionMessage::ModelSwitch { model, provider } => f
                .debug_struct("ModelSwitch")
                .field("model", model)
                .field("provider", provider)
                .finish(),
            SessionMessage::ReasoningEffort { effort } => f
                .debug_struct("ReasoningEffort")
                .field("effort", effort)
                .finish(),
            SessionMessage::UpdateRuntimeConfig {
                max_output_tokens,
                max_iterations,
                temperature,
                system_prompt_override,
                shell_approval_threshold,
            } => f
                .debug_struct("UpdateRuntimeConfig")
                .field("max_output_tokens", max_output_tokens)
                .field("max_iterations", max_iterations)
                .field("temperature", temperature)
                .field("has_system_prompt", &system_prompt_override.is_some())
                .field("shell_approval_threshold", shell_approval_threshold)
                .finish(),
            SessionMessage::UpdateWorkspaceContext { context_text } => f
                .debug_struct("UpdateWorkspaceContext")
                .field("len", &context_text.len())
                .finish(),
            SessionMessage::UpdateMcpTools { mcp_tools } => f
                .debug_struct("UpdateMcpTools")
                .field(
                    "mcp_tool_count",
                    &mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
                )
                .finish(),
            SessionMessage::UpdateSessionTitle { title } => f
                .debug_struct("UpdateSessionTitle")
                .field("title", title)
                .finish(),
            SessionMessage::SetWorkspaceId { workspace_id } => f
                .debug_struct("SetWorkspaceId")
                .field("workspace_id", workspace_id)
                .finish(),
            SessionMessage::SetWorkDir { path } => {
                f.debug_struct("SetWorkDir").field("path", path).finish()
            }
            SessionMessage::SetWorkspacePromptFile { content } => f
                .debug_struct("SetWorkspacePromptFile")
                .field("has_content", &content.is_some())
                .field("content_len", &content.as_ref().map(|c| c.len()))
                .finish(),
            SessionMessage::UpdateIdentityContext { identity_context } => f
                .debug_struct("UpdateIdentityContext")
                .field("has_identity", &identity_context.is_some())
                .finish(),
            SessionMessage::ProviderListUpdated => f.debug_struct("ProviderListUpdated").finish(),
            SessionMessage::Stop { reason } => {
                f.debug_struct("Stop").field("reason", reason).finish()
            }
            SessionMessage::EnableDebugMode(_) => f.debug_tuple("EnableDebugMode").finish(),
            SessionMessage::Close => f.debug_tuple("Close").finish(),
            SessionMessage::CompactContext => f.debug_tuple("CompactContext").finish(),
            SessionMessage::UpdateEmbedConfig {
                embed_endpoint,
                embed_model_id,
                embed_dimension,
            } => f
                .debug_struct("UpdateEmbedConfig")
                .field("embed_endpoint", embed_endpoint)
                .field("embed_model_id", embed_model_id)
                .field("embed_dimension", embed_dimension)
                .finish(),
            SessionMessage::SystemNotification { content } => f
                .debug_struct("SystemNotification")
                .field("len", &content.len())
                .finish(),
        }
    }
}

/// Independent execution actor for a single session.
///
/// Each `SessionTask` runs as a separate tokio task, processing
/// `SessionMessage`s from its inbound channel. It owns an `AgentLoop`
/// built from a cloned `AgentCore` plus its own `SessionState`,
/// ensuring full per-session isolation.
pub(crate) struct SessionTask {
    /// The session's AgentLoop, pre-constructed so that external callers
    /// can obtain its `InboundMessage` sender at session-creation time.
    agent_loop: AgentLoop,
    /// Clone of the AgentLoop's inbound sender, kept here purely as a
    /// fallback so that legacy `SessionMessage::ContinueExecution` /
    /// `SessionMessage::Stop` messages (if anyone still sends them)
    /// can be forwarded. The primary, deadlock-safe path is via
    /// `SessionHandle::send_inbound`.
    agent_inbound_tx: mpsc::Sender<InboundMessage>,
    /// Inbound message receiver (SessionMessage-level, not InboundMessage)
    inbound_rx: mpsc::Receiver<SessionMessage>,
    /// System prompt for context building
    system_prompt: String,
    /// Optional streaming chunk sender for forwarding responses to Gateway
    chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Dedicated control-event sender for Done/Error/Stopped events.
    /// Separated from `chunk_tx` to guarantee control events are never
    /// blocked by data-event backpressure.
    control_chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Unique session identifier (used for logging and chunk tagging)
    session_id: String,
    /// Complete tool definitions (with input_schema) for ContextBuilder
    tool_definitions: Vec<serde_json::Value>,
    /// Identity context string injected by Gateway
    identity_context: Option<String>,
    /// LLM protocol type (for image token estimation)
    protocol_type: acowork_core::protocol::ProtocolType,
}

/// Extract text from a document file directly, bypassing PathGuardedTool.
///
/// Used during session message pre-processing to read user-uploaded documents
/// from the session documents directory — which is NOT a workspace directory
/// and would be rejected by `PathGuardedTool::validate_path()`.
///
/// Delegates to the doc_reader format-specific extractors in a `spawn_blocking`
/// worker so that PDF rendering never blocks the async runtime.
async fn extract_document_text(path: &std::path::Path) -> Result<String, String> {
    let format = detect_format(path).ok_or_else(|| {
        format!(
            "Unsupported document format: {}",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("(none)")
        )
    })?;

    let opts = ExtractOptions {
        start_page: None,
        end_page: None,
        include_tables: true,
    };

    let path_clone = path.to_path_buf();
    let opts_clone = opts.clone();

    tokio::task::spawn_blocking(move || match format {
        "pdf" => doc_reader::pdf::extract_text(&path_clone, &opts_clone),
        "docx" => doc_reader::docx::extract_text(&path_clone, &opts_clone),
        "pptx" => doc_reader::pptx::extract_text(&path_clone, &opts_clone),
        "xlsx" => doc_reader::xlsx::extract_text(&path_clone, &opts_clone),
        _ => unreachable!(),
    })
    .await
    .map_err(|e| format!("Document extraction error: {e}"))
    .and_then(|r| r)
}

/// Build context hints from attached context items.
///
/// Instead of reading file contents, this function only emits file-path
/// references (with optional line ranges for selections) so the LLM can
/// use its own tools (`read_file` for text files, `doc_reader` for binary
/// documents) to access the content on demand. This avoids loading
/// potentially large files into the prompt context and keeps the Runtime
/// free of file-I/O concerns.
fn build_attached_context_blocks(
    items: &[acowork_core::protocol::AttachedContextItem],
    session_id: &str,
) -> Vec<String> {
    let mut file_hints: Vec<String> = Vec::new();

    for item in items {
        // Only process file and selection types
        if item.context_type == "directory" {
            continue;
        }

        let path = std::path::Path::new(&item.abs_path);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let is_document = matches!(ext, "pdf" | "docx" | "pptx" | "xlsx");

        tracing::info!(
            session_id = %session_id,
            abs_path = %item.abs_path,
            context_type = %item.context_type,
            is_document = is_document,
            "SessionTask: attaching file reference"
        );

        let line_hint = match (item.start_line, item.end_line) {
            (Some(s), Some(e)) if s != e => {
                format!(" (lines {}–{})", s, e)
            }
            (Some(s), _) => format!(" (line {})", s),
            _ => String::new(),
        };

        let has_line_range = item.context_type == "selection"
            && (item.start_line.is_some() || item.end_line.is_some());

        let hint = if is_document {
            // Binary documents (PDF, DOCX, etc.): always use doc_reader.
            // Line-level selection does not apply to binary formats.
            format!(
                "- `{}` — Use `doc_reader` to extract text from this document.",
                item.abs_path
            )
        } else if has_line_range {
            format!(
                "- `{}`{} — Use `read_file` to read the specified lines.",
                item.abs_path, line_hint
            )
        } else {
            format!(
                "- `{}` — Use `read_file` to read this file when needed.",
                item.abs_path
            )
        };
        file_hints.push(hint);
    }

    file_hints
}

impl SessionTask {
    /// Create a new SessionTask with the given shared core, session state,
    /// message receiver, system prompt, and optional chunk channel.
    ///
    /// Returns the task together with the `AgentLoop`'s `InboundMessage`
    /// sender. Callers (SessionManager) must stash that sender in
    /// `SessionHandle` so that out-of-band signals (Continue/Interrupt)
    /// can be delivered directly to the AgentLoop without going through
    /// the SessionTask's main loop — which would otherwise deadlock
    /// whenever the AgentLoop is awaiting a pause-resume signal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        core: Arc<AgentCore>,
        session: SessionState,
        inbound_rx: mpsc::Receiver<SessionMessage>,
        system_prompt: String,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        control_chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        session_id: String,
        tool_definitions: Vec<serde_json::Value>,
        identity_context: Option<String>,
        protocol_type: acowork_core::protocol::ProtocolType,
        mcp_tools: Option<Vec<Arc<dyn Tool>>>,
        runtime_debug: Option<DebugHandles>,
        pending_debug_handles: Arc<tokio::sync::Mutex<Option<DebugHandles>>>,
        // Accumulated runtime config overrides from Gateway pushes.
        // Applied directly to AgentCore during session init so the session
        // starts with correct values (not patched via message replay).
        runtime_overrides: RuntimeConfigOverrides,
        // Resolved workspace directory for tool execution.
        // None = keep the default from AgentCore.config.work_dir.
        initial_work_dir: Option<String>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        // Build the AgentLoop eagerly so its inbound sender can be exposed.
        // Heavy fields (provider, tools) are Arc-cloned (refcount only).
        let mut core_for_session =
            core.clone_for_session(chunk_tx.clone(), control_chunk_tx.clone(), session_id.clone());
        // Set MCP tools and rebuild the merged dispatch list
        core_for_session.mcp_tools = mcp_tools;
        core_for_session.rebuild_all_tools();

        // Inject the shared pending-debug-handles channel so SessionManager
        // can bypass the message queue when enabling debug mode on a running
        // session (whose message loop is blocked on agent_loop.run().await).
        core_for_session.set_debug_pending_injection(pending_debug_handles);

        // Inject runtime debug handles into the session's core if provided.
        // This enables debug mode on sessions created AFTER Gateway pushes
        // EnableDebugMode, without requiring a process restart.
        if let Some(handles) = runtime_debug {
            let observer = DebugObserverImpl::new(handles);
            core_for_session.set_debug_mode(observer);
        }

        // ── Per-session field initialization ──
        // All fields that are per-session (not globally shared) must be
        // initialized from session creation context here, not patched later
        // via message replay.

        // ── 429 retry UX setup ──
        // Create shared state that bridges ReliableProvider (inside the
        // LLM call) with SessionTask (handling ContinueExecution from the
        // user via Gateway). Without this, the ReliableProvider has no way
        // to emit session status changes or listen for user skip signals.
        let retry_session_status = Arc::new(std::sync::RwLock::new(
            crate::agent::session_state::SessionStatus::Streaming { message_id: None },
        ));
        let retry_wait_handle = crate::providers::reliable::RetryWaitHandle::new();
        core_for_session.retry_session_status = Some(retry_session_status);
        core_for_session.retry_wait_handle = Some(retry_wait_handle);

        // ADR-012: Rebuild LLM Provider from SessionState.provider so the
        // session always sends requests to the correct API endpoint.
        // Without this, clone_for_session inherits the global startup provider.
        //
        // The rebuilt provider is wrapped in ReliableProvider (retry logic)
        // AND wired up with 429-retry UX (countdown + skip button) via
        // `build_provider_for`, which checks `retry_session_status` and
        // `retry_wait_handle` set above.
        if let Some(ref provider_id) = session.provider
            && let Some(new_provider) = core_for_session.build_provider_for(provider_id) {
                let model = session.model.clone().unwrap_or_default();
                core_for_session.update_provider(new_provider, model);
            } else {
                // For new sessions (no per-session provider override),
                // re-wrap the startup provider with ReliableProvider + UX.
                let raw = core_for_session.provider.clone();
                let retry_config = crate::providers::reliable::RetryConfig::default();
                let mut reliable = crate::providers::reliable::ReliableProvider::new(
                    raw,
                    retry_config,
                );
                // Wire up UX if shared state is available
                if let Some(status) = &core_for_session.retry_session_status
                    && let Some(handle) = &core_for_session.retry_wait_handle
                    && let Some(tx) = &core_for_session.on_chunk
                    && let Some(sid) = &core_for_session.session_id
                {
                    reliable = reliable.with_retry_ux(
                        crate::providers::reliable::RetryWaitHandle {
                            state: handle.state.clone(),
                            skip_notify: handle.skip_notify.clone(),
                        },
                        status.clone(),
                        tx.clone(),
                        sid.clone(),
                    );
                }
                let model = session.model.clone().unwrap_or_default();
                core_for_session.update_provider(Arc::new(reliable), model);
            }

        // Apply accumulated runtime config overrides from Gateway pushes.
        // (temperature_override, system_prompt_override, shell_approval_threshold,
        //  max_iterations, max_output_tokens)
        core_for_session.apply_runtime_config(
            runtime_overrides.max_output_tokens,
            runtime_overrides.max_iterations,
            runtime_overrides.temperature,
            runtime_overrides.system_prompt_override,
            runtime_overrides.shell_approval_threshold,
        );

        // Sync agent-level temperature override to per-session state so it
        // appears in session_state_changed events and is visible in the frontend.
        let mut session = session;
        if core_for_session.temperature_override.is_some() {
            session.set_temperature(core_for_session.temperature_override);
        }

        // Set initial workspace directory for tool execution.
        if let Some(dir) = initial_work_dir {
            core_for_session.current_work_dir = Some(dir);
        }

        let (agent_loop, agent_inbound_tx) =
            AgentLoop::from_core_and_session(core_for_session, session);

        let task = Self {
            agent_loop,
            agent_inbound_tx: agent_inbound_tx.clone(),
            inbound_rx,
            system_prompt,
            chunk_tx,
            control_chunk_tx,
            session_id,
            tool_definitions,
            identity_context,
            protocol_type,
        };
        (task, agent_inbound_tx)
    }

    /// Set the status watch sender (ADR-014).
    /// Called by SessionManager after creating the SessionTask, before spawning.
    pub(crate) fn set_status_tx(
        &mut self,
        tx: tokio::sync::watch::Sender<crate::agent::session_state::SessionStatus>,
    ) {
        self.agent_loop.core.status_tx = Some(tx);
    }

    /// Set the shared snapshot slot for the Gateway pull API.
    ///
    /// Called by SessionManager after creating the SessionTask, before spawning.
    /// The slot is a shared `Arc<RwLock<Option<SessionStateSnapshot>>>` that
    /// `AgentLoop::emit_session_state` writes to on every status transition,
    /// and `SessionManager::snapshot_session_state` reads from.
    pub(crate) fn set_snapshot_slot(
        &mut self,
        slot: Arc<std::sync::RwLock<Option<crate::agent::session_state::SessionStateSnapshot>>>,
    ) {
        self.agent_loop.core.snapshot_slot = Some(slot);
    }

    /// Return the per-session urgent_stop Notify so SessionManager can
    /// route fire_urgent_stop() to only the target session.
    /// Returns None in standalone mode (where urgent_stop is not initialized).
    pub(crate) fn urgent_stop_notify(&self) -> Option<Arc<Notify>> {
        self.agent_loop.core.urgent_stop.clone()
    }

    /// Run the session task, processing messages until Stop or channel close.
    pub async fn run(self) {
        let Self {
            mut agent_loop,
            agent_inbound_tx,
            session_id,
            chunk_tx,
            control_chunk_tx,
            mut inbound_rx,
            system_prompt,
            tool_definitions,
            identity_context,
            protocol_type,
        } = self;

        // Build ContextBuilder with complete tool definitions and identity
        // from SessionManagerConfig, instead of building simplified ones from manifest.
        let mut context_builder = ContextBuilder::new(system_prompt.clone())
            .with_identity(identity_context.clone())
            .with_tools(tool_definitions.clone());

        // Mirror the identity onto SessionState so compaction paths
        // (loop_context / loop_session) can inject the user's preferred
        // language into the compact model's system prompt.
        agent_loop.session.set_identity_context(identity_context.clone());

        // ADR-012: Apply per-session model from SessionState.
        // For new sessions, model is set from resource_cache during creation.
        // For resumed sessions, model is restored from JSONL metadata.
        if let Some(ref model) = agent_loop.session.model {
            context_builder = context_builder.with_override_model(model.clone());
        }

        // Set protocol type for image token estimation in HistoryManager.
        agent_loop
            .session
            .history_mut()
            .set_protocol_type(protocol_type.clone());

        // Emit initial session state so the snapshot_slot is populated
        // before the frontend's first fetchSessionState pull request.
        // Without this, the slot stays None until the first status transition
        // or ProviderListUpdated message, causing the frontend to see null
        // for reasoning_effort and hide the thinking level control.
        agent_loop.emit_session_state();

        // Emit initial context-usage indicator for resumed sessions so the
        // frontend can show input/output token counts without waiting for
        // the first LLM round. Only fires when persisted last_tokens exist
        // and model capabilities are available.
        if let Some(ref conv) = agent_loop.session.conversation
            && let Some((input, output)) = conv.last_tokens()
        {
            let model_name = agent_loop
                .session
                .model
                .as_deref()
                .unwrap_or("unknown");
            if let Some(caps) = agent_loop.core.get_model_capabilities(model_name) {
                let max_output = agent_loop
                    .core
                    .max_output_tokens_limit_for_model(model_name);
                let ctx = crate::agent::context::build_context_usage_from_persisted(
                    &caps,
                    input,
                    output,
                    max_output,
                );
                if let Some(ref tx) = chunk_tx {
                    let _ = tx
                        .send(SessionChunkEvent {
                            session_id: session_id.clone(),
                            event: ChunkEvent::ContextUsage(ctx),
                        })
                        .await;
                }
            }
        }

        // Saved user message for debug resume re-execution.
        // When the user presses resume after the agent loop has exited
        // (e.g. after rewind was issued post-completion), SessionTask
        // replays the agent loop with this saved message.
        let mut last_user_message: Option<(String, String)> = None;

        loop {
            // Use tokio::select! to await inbound messages, rewind
            // notifications, and resume notifications — all sourced
            // from the debug observer slot (ADR-013).
            let msg = if let Some(rewind) = agent_loop.core.debug_observer.rewind_notify().cloned()
            {
                let resume = agent_loop
                    .core
                    .debug_observer
                    .resume_notify()
                    .cloned()
                    .expect("resume_notify must be set when rewind_notify is set");
                tokio::select! {
                    msg = inbound_rx.recv() => msg,
                    _ = rewind.notified() => {
                        // Apply rewind via the observer
                        agent_loop.core.debug_observer.apply_rewind(
                            &session_id,
                            &mut agent_loop.session.history,
                        ).await;
                        continue;
                    }
                    _ = resume.notified() => {
                        // Resume or Step pressed while agent loop is not running.
                        let can_continue = if let Some(ctrl) = agent_loop.core.debug_observer.debug_ctrl() {
                            let guard = ctrl.lock().await;
                            matches!(
                                guard.state,
                                crate::debug::controller::DebugState::Running
                                    | crate::debug::controller::DebugState::Stepping
                            )
                        } else {
                            false
                        };
                        if can_continue
                            && let Some((ref content, ref msg_id)) = last_user_message
                        {
                                tracing::info!(
                                    session_id = %session_id,
                                    "Debug: resume/step notify — restarting agent loop"
                                );
                                // Apply rewind/patches before run
                                agent_loop.core.debug_observer.apply_rewind_and_patches(
                                    &session_id,
                                    &mut agent_loop.session.history,
                                    &mut context_builder,
                                ).await;
                                // Use replay() to avoid appending a duplicate user message
                                // to history (the original is already there).
                                match agent_loop.replay(content, &mut context_builder, None).await {
                                    Ok(response) => {
                                        tracing::info!(
                                            session_id = %session_id,
                                            response_len = response.len(),
                                            "SessionTask processed chat message (replay)"
                                        );
                                        if let Some(ref tx) = control_chunk_tx {
                                            let event = SessionChunkEvent {
                                                session_id: session_id.clone(),
                                                event: ChunkEvent::Done {
                                                    content: response,
                                                    message_id: msg_id.clone(),
                                                },
                                            };
                                            if tx.send(event).await.is_err() {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    "Failed to send Done chunk event (replay)"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            session_id = %session_id,
                                            error = %e,
                                            "SessionTask agent loop error (replay)"
                                        );
                                        if let Some(ref tx) = control_chunk_tx {
                                            let (user_message, detail, error_type) = e.error_info();
                                            let event = SessionChunkEvent {
                                                session_id: session_id.clone(),
                                                event: ChunkEvent::Error {
                                                    user_message,
                                                    detail,
                                                    error_type,
                                                    message_id: msg_id.clone(),
                                                },
                                            };
                                            if tx.send(event).await.is_err() {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    "Failed to send Error chunk event (replay)"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        continue;
                    }
                }
            } else {
                inbound_rx.recv().await
            };

            // Note: msg is now Option<SessionMessage> directly (no
            // Ok/Err wrapper from the old timeout pattern).
            match msg {
                Some(SessionMessage::ChatMessage {
                    content,
                    message_id,
                    skill_instructions,
                    documents,
                    content_parts,
                    attached_context,
                }) => {
                    let has_documents = documents.as_ref().is_some_and(|d| !d.is_empty());
                    let has_content_parts = content_parts.as_ref().is_some_and(|p| !p.is_empty());
                    let has_attached = attached_context.as_ref().is_some_and(|a| !a.is_empty());
                    if content.trim().is_empty()
                        && !has_documents
                        && !has_content_parts
                        && !has_attached
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            "SessionTask received empty chat message, ignoring"
                        );
                        continue;
                    }

                    // Save the user message so it can be replayed if
                    // resume is pressed after the agent loop exits
                    // (e.g. after a rewind issued post-completion).
                    last_user_message = Some((content.clone(), message_id.clone()));

                    // Persist document upload records to the conversation JSONL
                    // before running the agent loop, so they appear in session history.
                    if let Some(ref docs) = documents
                        && !docs.is_empty() {
                            agent_loop.write_document_entries(docs);
                        }

                    // Build enriched user message: pre-extract user-uploaded document
                    // content via doc_reader tool (simulating an LLM tool call) and
                    // inject directly into context. This avoids an extra LLM round-trip
                    // and eliminates the uncertainty of whether the LLM will call
                    // doc_reader. The doc_reader tool remains available for
                    // non-user-uploaded documents (e.g., files in workspace).
                    let mut enriched_content = content.clone();
                    if let Some(ref docs) = documents
                        && !docs.is_empty() {
                            let filenames: Vec<&str> = docs
                                .iter()
                                .filter_map(|d| d.get("filename").and_then(|v| v.as_str()))
                                .collect();
                            tracing::info!(
                                session_id = %session_id,
                                doc_count = docs.len(),
                                filenames = ?filenames,
                                "SessionTask: pre-extracting uploaded documents via doc_reader"
                            );
                            let mut doc_blocks: Vec<String> = Vec::new();
                            for doc in docs {
                                let abs_path =
                                    doc.get("abs_path").and_then(|v| v.as_str()).unwrap_or("");
                                let filename = doc
                                    .get("filename")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("document");
                                if abs_path.is_empty() {
                                    continue;
                                }
                                let format = doc
                                    .get("format")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                tracing::info!(
                                    session_id = %session_id,
                                    filename = %filename,
                                    format = %format,
                                    abs_path = %abs_path,
                                    "SessionTask: extracting document"
                                );
                                let doc_path = std::path::Path::new(abs_path);
                                // Bypass PathGuardedTool: session documents dir is NOT a
                                // workspace directory, but the user explicitly uploaded
                                // these files — they are trusted input.
                                match extract_document_text(doc_path).await {
                                    Ok(text) if !text.trim().is_empty() => {
                                        doc_blocks.push(format!(
                                            "<attached_document filename=\"{}\" format=\"{}\">\n{}\n</attached_document>",
                                            filename, format, text
                                        ));
                                    }
                                    Ok(_) => {
                                        tracing::warn!(filename = %filename, "doc_reader returned empty content");
                                        doc_blocks.push(format!(
                                            "<attached_document filename=\"{}\" format=\"{}\">\n[Document is empty or contains no extractable text]\n</attached_document>",
                                            filename, format
                                        ));
                                    }
                                    Err(e) => {
                                        tracing::warn!(filename = %filename, error = %e, "Failed to extract document via doc_reader");
                                        doc_blocks.push(format!(
                                            "<attached_document filename=\"{}\" format=\"{}\">\n[Document extraction failed: {}]\n</attached_document>",
                                            filename, format, e
                                        ));
                                    }
                                }
                            }
                            if !doc_blocks.is_empty() {
                                let prefix = if content.trim().is_empty() {
                                    String::new()
                                } else {
                                    format!("{}\n\n", content)
                                };
                                enriched_content = format!(
                                    "{}The following documents were uploaded by the user. \
                                     Their contents have been pre-extracted and included below. \
                                     You do NOT need to use the `doc_reader` tool for these files.\n\n{}",
                                    prefix,
                                    doc_blocks.join("\n\n")
                                );
                            }
                            tracing::info!(
                                session_id = %session_id,
                                doc_blocks = doc_blocks.len(),
                                enriched_len = enriched_content.len(),
                                "SessionTask: document pre-extraction complete"
                            );
                        }

                    // Build attached context: emit file-path references for
                    // workspace files selected by the user from the workspace
                    // explorer or editor "Add to Chat" button. The LLM will use
                    // its own tools (read_file, doc_reader) to access file
                    // contents on demand, avoiding large file injection into the
                    // prompt context.
                    //
                    // The frontend sends absolute paths (abs_path).
                    if let Some(ref att_ctx) = attached_context
                        && !att_ctx.is_empty() {
                            tracing::info!(
                                session_id = %session_id,
                                count = att_ctx.len(),
                                "SessionTask: building attached file references"
                            );

                            let file_hints = build_attached_context_blocks(att_ctx, &session_id);

                            if !file_hints.is_empty() {
                                let prefix = if enriched_content.trim().is_empty() {
                                    String::new()
                                } else {
                                    format!("{}\n\n", enriched_content)
                                };
                                enriched_content = format!(
                                    "{}The following workspace files were attached by the user. \
                                     Use the suggested tools to read them when you need their contents.\n\n{}",
                                    prefix,
                                    file_hints.join("\n")
                                );
                            }
                            tracing::info!(
                                session_id = %session_id,
                                file_hints = file_hints.len(),
                                enriched_len = enriched_content.len(),
                                "SessionTask: attached file references built"
                            );
                        }

                    // Apply skill instructions to ContextBuilder (system prompt injection).
                    // This replaces the old behavior of prepending skill text to the user message,
                    // making skill instructions visible in the debug panel's system prompt section.
                    // When skill_instructions is None (no command specified), clear any
                    // previously set skill to prevent stale instructions leaking across turns.
                    if let Some(ref instructions) = skill_instructions {
                        tracing::info!(
                            session_id = %session_id,
                            skill_len = instructions.len(),
                            "Applying skill instructions to ContextBuilder"
                        );
                        context_builder.set_skill_instructions(instructions.clone());
                    } else {
                        context_builder.clear_skill_instructions();
                    }

                    // ── Debug mode: apply rewind/patches before running agent loop ──
                    agent_loop
                        .core
                        .debug_observer
                        .apply_rewind_and_patches(
                            &session_id,
                            &mut agent_loop.session.history,
                            &mut context_builder,
                        )
                        .await;

                    // ── Debug mode: auto-resume if paused/stopped ──
                    // When the user sends a chat message while the debug controller
                    // is Paused, the agent loop is blocking in await_step_or_continue()
                    // on rewind_notify (polled every 100ms).  Switch to Running so the
                    // next poll sees the new state and continues processing.
                    //
                    // We do NOT match Stepping here — stepping is a deliberate mode
                    // where each phase step auto-pauses (on_phase_step_done → Paused).
                    // Overriding Stepping to Running would defeat the user's intent to
                    // single-step through the agent's reasoning.
                    //
                    // We do NOT call resume_notify.notify_one() — the paused agent loop
                    // waits on rewind_notify (polling), not resume_notify.  Calling
                    // notify_one() here would leak a permit to the next iteration of
                    // the SessionTask loop, causing an unwanted replay of the last
                    // user message via the resume.notified() branch.
                    if let Some(ctrl) = agent_loop.core.debug_observer.debug_ctrl() {
                        let mut guard = ctrl.lock().await;
                        match guard.state {
                            crate::debug::controller::DebugState::Paused
                            | crate::debug::controller::DebugState::Stopped => {
                                let old_state = guard.state;
                                guard.state = crate::debug::controller::DebugState::Running;
                                let iteration = guard.iteration;
                                drop(guard);
                                tracing::info!(
                                    session_id = %session_id,
                                    old_state = ?old_state,
                                    "Debug: auto-resuming on chat_message"
                                );
                                // Notify the debug frontend so it updates the UI
                                if let Some(event_tx) =
                                    agent_loop.core.debug_observer.debug_event_tx()
                                {
                                    let _ = event_tx.send(
                                        crate::debug::server::DebugEvent::ExecutionStateChanged {
                                            new_state:
                                                crate::debug::controller::DebugState::Running,
                                            iteration,
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }

                    // Check for bypass-injected debug handles before each agent
                    // loop run (safety net for idle sessions).
                    agent_loop.core.debug_observer.check_pending_injection();

                    match agent_loop
                        .run(&enriched_content, &mut context_builder, content_parts)
                        .await
                    {
                        Ok(response) => {
                            tracing::info!(
                                session_id = %session_id,
                                response_len = response.len(),
                                "SessionTask processed chat message"
                            );
                            if let Some(ref tx) = control_chunk_tx {
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Done {
                                        content: response,
                                        message_id,
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Done chunk event"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %session_id,
                                error = %e,
                                "SessionTask agent loop error"
                            );
                            if let Some(ref tx) = control_chunk_tx {
                                let (user_message, detail, error_type) = e.error_info();
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Error {
                                        user_message,
                                        detail,
                                        error_type,
                                        message_id,
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Error chunk event"
                                    );
                                }
                            }
                        }
                    }
                }
                Some(SessionMessage::ContinueExecution) => {
                    tracing::debug!(
                        session_id = %session_id,
                        "SessionTask: ContinueExecution received"
                    );
                    // 429 retry UX: if the ReliableProvider is currently in a
                    // long retry wait, wake it immediately via skip_notify.
                    if let Some(ref handle) = agent_loop.core.retry_wait_handle {
                        handle.skip_notify.notify_one();
                        tracing::info!(
                            session_id = %session_id,
                            "Skip retry wait triggered via ContinueExecution"
                        );
                    }
                    let _ = agent_inbound_tx
                        .send(crate::agent::inbound::InboundMessage::ContinueExecution {
                            reason: "user_requested".to_string(),
                        })
                        .await;
                }
                Some(SessionMessage::ModelSwitch { model, provider }) => {
                    tracing::info!(
                        session_id = %session_id,
                        model = %model,
                        provider = ?provider,
                        "SessionTask: model switch requested (ADR-012: per-session)"
                    );
                    // Update in-memory SessionState
                    agent_loop.session.set_model(model.clone());
                    if let Some(ref p) = provider {
                        agent_loop.session.set_provider(p.clone());
                    }
                    // Persist to JSONL conversation file
                    if let Some(conv) = agent_loop.session.conversation() {
                        conv.update_model_provider(&model, provider.as_deref());
                    }
                    // If the provider also changed, rebuild the LLM Provider
                    // instance from the shared global cache (set by
                    // ProviderListUpdate / AgentHello). No per-session vault.
                    if let Some(ref provider_id) = provider {
                        if let Some(new_provider) = agent_loop.core.build_provider_for(provider_id)
                        {
                            agent_loop.update_provider(
                                new_provider,
                                model.clone(),
                                Some(provider_id.clone()),
                            );
                        } else {
                            tracing::warn!(
                                session_id = %session_id,
                                provider_id = %provider_id,
                                "ModelSwitch: provider not found in global cache, keeping current Provider instance"
                            );
                        }
                    }
                    // Update context builder for next iteration
                    context_builder.set_override_model(model.clone());
                    // Reset reasoning_effort to new model's default (clear user override).
                    // Three-level priority chain:
                    // 1. provider capabilities default_reasoning_effort
                    // 2. Auto (if supports_reasoning is true)
                    // 3. None (provider does not support thinking control)
                    // No persisted session value applies on model switch — the model changed.
                    let caps = agent_loop.core.get_model_capabilities(&model);
                    let provider_default = caps
                        .as_ref()
                        .and_then(|c| c.default_reasoning_effort.clone());
                    let default_effort = provider_default
                        .as_deref()
                        .and_then(acowork_core::providers::traits::ReasoningEffort::from_str_loose)
                        .or_else(|| {
                            if caps.as_ref().and_then(|c| c.supports_reasoning).unwrap_or(false) {
                                Some(acowork_core::providers::traits::ReasoningEffort::Auto)
                            } else {
                                None
                            }
                        });
                    agent_loop.session.set_reasoning_effort(default_effort.clone());
                    // Persist new effort to ConversationSession so resume is consistent.
                    if let Some(conv) = agent_loop.session.conversation() {
                        let effort_str = default_effort.as_ref().map(|e| e.to_string());
                        conv.update_reasoning_effort(effort_str);
                    }
                }
                Some(SessionMessage::ReasoningEffort { effort }) => {
                    let parsed = acowork_core::providers::traits::ReasoningEffort::from_str_loose(&effort);
                    tracing::info!(
                        session_id = %session_id,
                        effort = %effort,
                        parsed = ?parsed,
                        "SessionTask: reasoning effort override"
                    );
                    agent_loop.session.set_reasoning_effort(parsed);
                    // Persist to JSONL so the override survives session resume.
                    if let Some(conv) = agent_loop.session.conversation() {
                        conv.update_reasoning_effort(Some(effort));
                    }
                }
                Some(SessionMessage::UpdateRuntimeConfig {
                    max_output_tokens,
                    max_iterations,
                    temperature,
                    system_prompt_override,
                    shell_approval_threshold,
                }) => {
                    tracing::info!(
                        session_id = %session_id,
                        max_output_tokens = ?max_output_tokens,
                        max_iterations = ?max_iterations,
                        temperature = ?temperature,
                        "SessionTask: applying runtime config overrides"
                    );
                    agent_loop.apply_runtime_config(
                        max_output_tokens,
                        max_iterations,
                        temperature,
                        system_prompt_override,
                        shell_approval_threshold,
                    );
                }
                Some(SessionMessage::UpdateWorkspaceContext { context_text }) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: updating workspace context"
                    );
                    context_builder.set_workspace_context(context_text);
                }
                Some(SessionMessage::UpdateMcpTools { mcp_tools }) => {
                    tracing::info!(
                        session_id = %session_id,
                        mcp_tool_count = mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
                        "SessionTask: updating MCP tools on AgentCore"
                    );
                    agent_loop.core.mcp_tools = mcp_tools;
                    agent_loop.core.rebuild_all_tools();
                }
                Some(SessionMessage::UpdateSessionTitle { title }) => {
                    tracing::info!(
                        session_id = %session_id,
                        title = %title,
                        "SessionTask: updating session title"
                    );
                    let _ = agent_loop.update_session_title(&title);
                }
                Some(SessionMessage::SetWorkspaceId { workspace_id }) => {
                    tracing::info!(
                        session_id = %session_id,
                        workspace_id = %workspace_id,
                        "SessionTask: persisting workspace_id to JSONL"
                    );
                    agent_loop.update_session_workspace_id(&workspace_id);
                }
                Some(SessionMessage::SetWorkDir { path }) => {
                    tracing::info!(
                        session_id = %session_id,
                        path = %path,
                        "SessionTask: updating work_dir for tool execution"
                    );
                    agent_loop.core.current_work_dir = Some(path);
                }
                Some(SessionMessage::SetWorkspacePromptFile { content }) => {
                    tracing::info!(
                        session_id = %session_id,
                        has_content = content.is_some(),
                        "SessionTask: updating workspace prompt file content"
                    );
                    context_builder.set_workspace_prompt_file(content);
                }
                Some(SessionMessage::UpdateIdentityContext { identity_context }) => {
                    tracing::info!(
                        session_id = %session_id,
                        has_context = identity_context.is_some(),
                        "SessionTask: updating identity context"
                    );
                    let next = identity_context.unwrap_or_default();
                    context_builder.set_identity_context(next.clone());
                    // Keep SessionState in sync so compaction sees the latest value.
                    agent_loop.session.set_identity_context(
                        if next.is_empty() { None } else { Some(next) },
                    );
                }
                Some(SessionMessage::ProviderListUpdated) => {
                    // The shared global_provider_list on AgentCore is already updated
                    // (written by SessionManager before broadcasting this message).
                    // Just emit session state so the frontend sees the latest info.
                    // Reasoning_effort is NOT reset here — it was correctly initialized
                    // at session creation in build_initial_session_state, and the user
                    // may have explicitly overridden it via ReasoningEffort messages.
                    agent_loop.emit_session_state();
                }
                Some(SessionMessage::Stop { reason }) => {
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "SessionTask: forwarding stop signal"
                    );
                    let _ = agent_inbound_tx
                        .send(crate::agent::inbound::InboundMessage::Stop { reason })
                        .await;
                }
                Some(SessionMessage::EnableDebugMode(handles)) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: injecting debug mode into existing session"
                    );
                    // Create a DevMode observer from the handles and inject it
                    // into AgentCore (ADR-013: Observer Pipeline).
                    let observer = DebugObserverImpl::new(handles);
                    agent_loop.core.set_debug_mode(observer);
                }
                Some(SessionMessage::Close) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: Close received, shutting down"
                    );
                    break;
                }
                Some(SessionMessage::CompactContext) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: manual compact_context triggered"
                    );
                    let model_name = agent_loop.session.model().unwrap_or("default").to_string();
                    agent_loop
                        .compact_history_if_needed(&model_name, true)
                        .await;
                }
                Some(SessionMessage::UpdateEmbedConfig {
                    embed_endpoint,
                    embed_model_id,
                    embed_dimension,
                }) => {
                    tracing::info!(
                        session_id = %session_id,
                        endpoint = %embed_endpoint,
                        model_id = %embed_model_id,
                        dimension = embed_dimension,
                        "SessionTask: updating embedding provider"
                    );

                    // Check if dimension migration is needed.
                    // When the Grafeo store already has data with a different
                    // dimension, we must re-embed all nodes and rebuild the
                    // HNSW indexes before switching to the new provider.
                    let needs_migration = agent_loop
                        .core
                        .memory_store
                        .as_ref()
                        .map(|store| store.embedding_dim() != embed_dimension)
                        .unwrap_or(false);

                    if needs_migration
                        && let Some(ref store) = agent_loop.core.memory_store
                    {
                            let store = store.clone();
                            let old_dim = store.embedding_dim();
                            tracing::info!(
                                old_dim,
                                new_dim = embed_dimension,
                                "Embedding dimension changed, starting migration"
                            );

                            // Build a temporary provider for the migration
                            // re-embedding. We use the new ONNX provider directly
                            // (not the fallback chain) to ensure consistent
                            // embeddings during migration.
                            let migration_provider =
                                crate::embedding::remote::RemoteEmbeddingProvider::with_config(
                                    &embed_endpoint,
                                    None,
                                    &embed_model_id,
                                    embed_dimension,
                                );
                            let migration_provider =
                                std::sync::Arc::new(migration_provider)
                                    as std::sync::Arc<dyn crate::embedding::EmbeddingProvider>;

                            // Bridge async embed into a sync closure for
                            // GrafeoStore::migrate_embedding_dimension.
                            let handle = tokio::runtime::Handle::current();
                            let provider_for_fn = migration_provider.clone();
                            let embed_fn = move |text: &str| -> Option<Vec<f32>> {
                                let text_owned = text.to_string();
                                match handle.block_on(provider_for_fn.embed(&text_owned)) {
                                    Ok(vec) => Some(vec),
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "Re-embedding failed during migration, skipping node"
                                        );
                                        None
                                    }
                                }
                            };

                            match store.migrate_embedding_dimension(embed_fn, embed_dimension) {
                                Ok(stats) => {
                                    tracing::info!(
                                        rebuilt = stats.rebuilt,
                                        skipped = stats.skipped_no_embedding + stats.skipped_no_content,
                                        errors = stats.errors,
                                        "Embedding migration complete"
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        "Embedding migration failed, vector search may be broken"
                                    );
                                }
                            }
                    }

                    // Build the new ONNX provider.
                    let new_onnx_provider =
                        crate::embedding::remote::RemoteEmbeddingProvider::with_config(
                            &embed_endpoint,
                            None,
                            &embed_model_id,
                            embed_dimension,
                        );
                    // Wrap as FallbackEmbeddingProvider with ONNX as primary,
                    // keeping the existing provider chain as fallback (if available).
                    // Lock the dimension so that fallback providers with a
                    // different dimension are automatically filtered out.
                    let new_emb: Arc<dyn crate::embedding::EmbeddingProvider> =
                        if let Some(ref old_provider) = agent_loop.core.embedding_provider {
                            Arc::new(crate::embedding::FallbackEmbeddingProvider::with_providers(
                                vec![
                                (Box::new(new_onnx_provider), 500),
                                (
                                    Box::new(
                                        crate::embedding::ArcDelegateEmbeddingProvider::from_arc(
                                            old_provider.clone(),
                                        ),
                                    ),
                                    5000,
                                ),
                            ],
                                crate::embedding::EmbeddingConfig::default(),
                            )
                            .with_locked_dimension(embed_dimension))
                        } else {
                            Arc::new(crate::embedding::FallbackEmbeddingProvider::with_providers(
                                vec![(Box::new(new_onnx_provider), 500)],
                                crate::embedding::EmbeddingConfig::default(),
                            )
                            .with_locked_dimension(embed_dimension))
                        };
                    agent_loop.core.update_embedding_provider(new_emb);
                }
                Some(SessionMessage::SystemNotification { content }) => {
                    // Only inject into sessions that have already started a conversation.
                    // Prevents MCP connection failure notifications from appearing before
                    // the user has sent their first message.
                    if agent_loop.session.history_mut().is_empty() {
                        tracing::debug!(
                            session_id = %session_id,
                            "SessionTask: skipping system notification — no conversation history yet"
                        );
                        continue;
                    }
                    tracing::info!(
                        session_id = %session_id,
                        content_len = content.len(),
                        "SessionTask: injecting system notification into history"
                    );
                    agent_loop
                        .session
                        .history_mut()
                        .append(ChatMessage::user(format!(
                            "[System Notification] {content}"
                        )));
                }
                None => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: inbound channel closed, shutting down"
                    );
                    break;
                }
            }
        }

        // Graceful shutdown: attempt to close session with distillation
        if let Err(e) = agent_loop.close_session_with_distillation().await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "SessionTask: failed to close session with distillation (non-fatal)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attached_context_file_reference() {
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/project/src/main.rs".to_string(),
            context_type: "file".to_string(),
            start_line: None,
            end_line: None,
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("/project/src/main.rs"));
        assert!(hints[0].contains("read_file"));
        assert!(!hints[0].contains("doc_reader"));
    }

    #[test]
    fn test_attached_context_selection_with_line_range() {
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/project/src/lib.rs".to_string(),
            context_type: "selection".to_string(),
            start_line: Some(2),
            end_line: Some(5),
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("/project/src/lib.rs"));
        assert!(hints[0].contains("lines 2–5"));
        assert!(hints[0].contains("read_file"));
    }

    #[test]
    fn test_attached_context_selection_single_line() {
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/project/src/main.rs".to_string(),
            context_type: "selection".to_string(),
            start_line: Some(42),
            end_line: Some(42),
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("line 42"));
        assert!(hints[0].contains("read_file"));
        // Should NOT say "line range" for a single-line selection
        assert!(!hints[0].contains("line range"));
    }

    #[test]
    fn test_attached_context_selection_without_line_range_falls_back() {
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/project/src/lib.rs".to_string(),
            context_type: "selection".to_string(),
            start_line: None,
            end_line: None,
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("/project/src/lib.rs"));
        assert!(hints[0].contains("read_file"));
        // Should NOT mention lines when no line range is provided
        assert!(!hints[0].contains("lines"));
        assert!(!hints[0].contains("line "));
    }

    #[test]
    fn test_attached_context_binary_document_uses_doc_reader() {
        for ext in &["pdf", "docx", "pptx", "xlsx"] {
            let items = vec![acowork_core::protocol::AttachedContextItem {
                abs_path: format!("/docs/report.{}", ext),
                context_type: "file".to_string(),
                start_line: None,
                end_line: None,
            }];

            let hints = build_attached_context_blocks(&items, "test-session");
            assert_eq!(hints.len(), 1, "failed for extension: {}", ext);
            assert!(
                hints[0].contains("doc_reader"),
                "expected doc_reader hint for {}, got: {}",
                ext,
                hints[0]
            );
            assert!(
                !hints[0].contains("read_file"),
                "should not mention read_file for {}, got: {}",
                ext,
                hints[0]
            );
        }
    }

    #[test]
    fn test_attached_context_binary_document_ignores_line_range() {
        // Even if the frontend sends a line range for a PDF,
        // we should still emit doc_reader (not read_file with lines).
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/docs/spec.pdf".to_string(),
            context_type: "selection".to_string(),
            start_line: Some(1),
            end_line: Some(10),
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("doc_reader"));
        assert!(!hints[0].contains("read_file"));
        assert!(!hints[0].contains("lines"));
    }

    #[test]
    fn test_attached_context_directory_skipped() {
        let items = vec![acowork_core::protocol::AttachedContextItem {
            abs_path: "/some/dir".to_string(),
            context_type: "directory".to_string(),
            start_line: None,
            end_line: None,
        }];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert!(hints.is_empty());
    }

    #[test]
    fn test_attached_context_multiple_files() {
        let items = vec![
            acowork_core::protocol::AttachedContextItem {
                abs_path: "/project/src/main.rs".to_string(),
                context_type: "file".to_string(),
                start_line: None,
                end_line: None,
            },
            acowork_core::protocol::AttachedContextItem {
                abs_path: "/project/src/lib.rs".to_string(),
                context_type: "selection".to_string(),
                start_line: Some(10),
                end_line: Some(20),
            },
            acowork_core::protocol::AttachedContextItem {
                abs_path: "/docs/guide.pdf".to_string(),
                context_type: "file".to_string(),
                start_line: None,
                end_line: None,
            },
        ];

        let hints = build_attached_context_blocks(&items, "test-session");
        assert_eq!(hints.len(), 3);
        assert!(hints[0].contains("read_file") && hints[0].contains("main.rs"));
        assert!(hints[1].contains("read_file") && hints[1].contains("lines 10–20"));
        assert!(hints[2].contains("doc_reader") && hints[2].contains("guide.pdf"));
    }
}
