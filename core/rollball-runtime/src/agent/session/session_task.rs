//! SessionTask: independent execution actor for a single session.
//!
//! Each `SessionTask` runs in its own tokio task, processing messages
//! from an inbound channel. It owns an `AgentLoop` instance for the
//! session's lifetime, ensuring per-session isolation of history,
//! budget, and loop detection while sharing provider/tools via Arc.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::agent::agent_core::AgentCore;
use crate::agent::context::ContextBuilder;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, SessionChunkEvent};
use crate::agent::session_state::SessionState;
use crate::debug::controller::DebugController;

/// Messages that can be sent to a SessionTask.
#[derive(Debug, Clone)]
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
    },
    /// Continue execution after tool result or iteration pause
    ContinueExecution,
    /// Switch the LLM model at runtime
    ModelSwitch { model: String },
    /// Update the LLM provider at runtime (hot-push from Gateway)
    UpdateProvider {
        provider_name: String,
        protocol_type: rollball_core::protocol::ProtocolType,
        api_key: Option<String>,
        base_url: Option<String>,
        model: String,
    },
    /// Update gateway model capabilities at runtime
    UpdateCapabilities {
        caps: rollball_core::protocol::ModelCapabilitiesInfo,
    },
    /// Update max output tokens limit from Gateway config
    UpdateMaxOutputTokens { limit: u64 },
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
    /// Update active tools (hot-push from Gateway RuntimeConfigUpdate).
    /// Carries the rebuilt tool_definitions JSON to replace in ContextBuilder.
    UpdateActiveTools {
        tool_definitions: Vec<serde_json::Value>,
    },
    /// Update the title of the session's conversation
    UpdateSessionTitle { title: String },
    /// Interrupt signal to stop the current agent loop iteration
    Interrupt { reason: String },
    /// Stop the session gracefully
    Stop,
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
    /// `SessionMessage::Interrupt` messages (if anyone still sends them)
    /// can be forwarded. The primary, deadlock-safe path is via
    /// `SessionHandle::send_inbound`.
    agent_inbound_tx: mpsc::Sender<InboundMessage>,
    /// Inbound message receiver (SessionMessage-level, not InboundMessage)
    inbound_rx: mpsc::Receiver<SessionMessage>,
    /// System prompt for context building
    system_prompt: String,
    /// Optional streaming chunk sender for forwarding responses to Gateway
    chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Unique session identifier (used for logging and chunk tagging)
    session_id: String,
    /// Complete tool definitions (with input_schema) for ContextBuilder
    tool_definitions: Vec<serde_json::Value>,
    /// Identity context string injected by Gateway
    identity_context: Option<String>,
    /// Model override from Gateway (takes precedence over manifest's suggested_model)
    override_model: Option<String>,
    /// Debug controller (shared across sessions, only in DevMode).
    /// Used to consume rewind_target / pending_patches / re_execute_pending
    /// before each agent_loop.run() invocation.
    debug_ctrl: Option<Arc<tokio::sync::Mutex<DebugController>>>,
    /// Debug rewind notification handle (shared, only in DevMode).
    ///
    /// The SessionTask's main loop uses `tokio::select!` to await
    /// this notify instead of polling, making rewind an event-driven
    /// operation rather than a polling-based side channel.
    rewind_notify: Option<Arc<Notify>>,
    /// Debug resume notification handle (shared, only in DevMode).
    ///
    /// When the user presses resume after the agent loop has exited
    /// (e.g. after rewind was issued post-completion), the resume
    /// handler calls `notify_one()` so the SessionTask can re-run
    /// the agent loop with the saved user message.
    resume_notify: Option<Arc<Notify>>,
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
        session_id: String,
        tool_definitions: Vec<serde_json::Value>,
        identity_context: Option<String>,
        override_model: Option<String>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        // Build the AgentLoop eagerly so its inbound sender can be exposed.
        // Heavy fields (provider, tools) are Arc-cloned (refcount only).
        // Extract debug_ctrl, rewind_notify, and resume_notify from the original
        // core BEFORE clone_for_session — both the AgentLoop (via clone_for_session)
        // and SessionTask hold an Arc to the same DebugController and Notify handles.
        let debug_ctrl = core.debug_ctrl().cloned();
        let rewind_notify = core.debug_rewind_notify().cloned();
        let resume_notify = core.debug_resume_notify().cloned();
        let core_for_session = core.clone_for_session(chunk_tx.clone(), session_id.clone());
        let (agent_loop, agent_inbound_tx) =
            AgentLoop::from_core_and_session(core_for_session, session);

        let task = Self {
            agent_loop,
            agent_inbound_tx: agent_inbound_tx.clone(),
            inbound_rx,
            system_prompt,
            chunk_tx,
            session_id,
            tool_definitions,
            identity_context,
            override_model,
            debug_ctrl,
            rewind_notify,
            resume_notify,
        };
        (task, agent_inbound_tx)
    }

    /// Set the status watch sender (ADR-014).
    /// Called by SessionManager after creating the SessionTask, before spawning.
    pub(crate) fn set_status_tx(&mut self, tx: tokio::sync::watch::Sender<crate::agent::session_state::SessionStatus>) {
        self.agent_loop.core.status_tx = Some(tx);
    }

    /// Run the session task, processing messages until Stop or channel close.
    pub async fn run(self) {
        let Self {
            mut agent_loop,
            agent_inbound_tx,
            debug_ctrl,
            rewind_notify,
            resume_notify,
            session_id,
            chunk_tx,
            mut inbound_rx,
            system_prompt,
            tool_definitions,
            identity_context,
            override_model,
        } = self;

        // Build ContextBuilder with complete tool definitions and identity
        // from SessionManagerConfig, instead of building simplified ones from manifest.
        let mut context_builder = ContextBuilder::new(system_prompt.clone())
            .with_identity(identity_context.clone())
            .with_tools(tool_definitions.clone());

        // Apply Gateway-resolved model override so the first message uses
        // the correct model (not the manifest's suggested_model fallback).
        if let Some(ref model) = override_model {
            context_builder = context_builder.with_override_model(model.clone());
        }

        // Saved user message for debug resume re-execution.
        // When the user presses resume after the agent loop has exited
        // (e.g. after rewind was issued post-completion), SessionTask
        // replays the agent loop with this saved message.
        let mut last_user_message: Option<(String, String)> = None;

        loop {
            // Use tokio::select! to await inbound messages, rewind
            // notifications, and resume notifications.
            //
            // When DevMode is disabled (rewind_notify is None),
            // inbound_rx is awaited directly.
            let msg = if let Some(ref rewind) = rewind_notify {
                // resume_notify is always Some when rewind_notify is
                // Some (both are set together in set_debug_mode).
                let resume = resume_notify.as_ref().expect("resume_notify must be set when rewind_notify is set");
                tokio::select! {
                    msg = inbound_rx.recv() => msg,
                    _ = rewind.notified() => {
                        // Apply rewind inline, then continue to the next
                        // iteration.  The same rewind may also be consumed
                        // by apply_debug_rewind_and_patches before
                        // agent_loop.run(), which is fine —
                        // take_rewind_target() returns None on the second
                        // call (idempotent).
                        if let Some(ref ctrl) = debug_ctrl {
                            apply_debug_rewind(
                                ctrl,
                                &session_id,
                                &mut agent_loop,
                            ).await;
                        }
                        continue;
                    }
                    _ = resume.notified() => {
                        // Resume pressed while agent loop is not running.
                        // Check if the debug state is Running and we have
                        // a saved user message to replay.
                        let should_replay = if let Some(ref ctrl) = debug_ctrl {
                            let guard = ctrl.lock().await;
                            guard.state == crate::debug::controller::DebugState::Running
                        } else {
                            false
                        };
                        if should_replay
                            && let Some((ref content, ref msg_id)) = last_user_message
                        {
                                tracing::info!(
                                    session_id = %session_id,
                                    "Debug: resume notify — replaying agent loop"
                                );
                                // Apply rewind/patches before replay
                                apply_debug_rewind_and_patches(
                                    &debug_ctrl,
                                    &session_id,
                                    &mut agent_loop,
                                    &mut context_builder,
                                ).await;
                                match agent_loop.replay(content, &mut context_builder).await {
                                    Ok(response) => {
                                        tracing::info!(
                                            session_id = %session_id,
                                            response_len = response.len(),
                                            "SessionTask processed chat message (replay)"
                                        );
                                        if let Some(ref tx) = chunk_tx {
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
                                        if let Some(ref tx) = chunk_tx {
                                            let event = SessionChunkEvent {
                                                session_id: session_id.clone(),
                                                event: ChunkEvent::Error {
                                                    message: format!("Error: {}", e),
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
                Some(SessionMessage::ChatMessage { content, message_id, skill_instructions, documents }) => {
                    if content.trim().is_empty() {
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
                    if let Some(ref docs) = documents {
                        if !docs.is_empty() {
                            agent_loop.write_document_entries(docs);
                        }
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
                    apply_debug_rewind_and_patches(
                        &debug_ctrl,
                        &session_id,
                        &mut agent_loop,
                        &mut context_builder,
                    )
                    .await;

                    // ── Debug mode: auto-resume if paused/stepping ──
                    // When the user sends a chat message, they expect a response.
                    // If the debug controller is Paused or Stepping, the agent loop
                    // will block at await_debug_resume().  Auto-resume so the
                    // message is processed immediately.
                    if let Some(ref ctrl) = debug_ctrl {
                        let mut guard = ctrl.lock().await;
                        match guard.state {
                            crate::debug::controller::DebugState::Paused
                            | crate::debug::controller::DebugState::Stepping => {
                                let old_state = guard.state.clone();
                                guard.state = crate::debug::controller::DebugState::Running;
                                let iteration = guard.iteration;
                                drop(guard);
                                tracing::info!(
                                    session_id = %session_id,
                                    old_state = ?old_state,
                                    "Debug: auto-resuming on chat_message"
                                );
                                // Notify the debug frontend so it updates the UI
                                if let Some(ref tx) = agent_loop.core.debug_event_tx() {
                                    let _ = tx.send(
                                        crate::debug::server::DebugEvent::ExecutionStateChanged {
                                            new_state: crate::debug::controller::DebugState::Running,
                                            iteration,
                                        },
                                    );
                                }
                                // Wake the agent loop's await_debug_resume() if it's
                                // currently blocking on the resume notify.
                                if let Some(ref notify) = resume_notify {
                                    notify.notify_one();
                                }
                            }
                            _ => {}
                        }
                    }

                    match agent_loop.run(&content, &mut context_builder).await {
                        Ok(response) => {
                            tracing::info!(
                                session_id = %session_id,
                                response_len = response.len(),
                                "SessionTask processed chat message"
                            );
                            if let Some(ref tx) = chunk_tx {
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
                            if let Some(ref tx) = chunk_tx {
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Error {
                                        message: format!("Error: {}", e),
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
                    let _ = agent_inbound_tx.send(crate::agent::inbound::InboundMessage::ContinueExecution {
                        reason: "user_requested".to_string(),
                    }).await;
                }
                Some(SessionMessage::ModelSwitch { model }) => {
                    tracing::info!(
                        session_id = %session_id,
                        model = %model,
                        "SessionTask: model switch requested"
                    );
                    context_builder.set_override_model(model);
                }
                Some(SessionMessage::UpdateProvider { provider_name, protocol_type, api_key, base_url, model }) => {
                    tracing::info!(
                        session_id = %session_id,
                        provider = %provider_name,
                        model = %model,
                        "SessionTask: updating provider"
                    );
                    let new_provider = crate::providers::router::create_provider(
                        &provider_name,
                        &protocol_type,
                        api_key.as_deref(),
                        base_url.as_deref(),
                    );
                    agent_loop.update_provider(new_provider, model);
                }
                Some(SessionMessage::UpdateCapabilities { caps }) => {
                    tracing::info!(
                        session_id = %session_id,
                        model = ?caps.name,
                        "SessionTask: updating model capabilities"
                    );
                    agent_loop.update_gateway_model_capabilities(caps);
                }
                Some(SessionMessage::UpdateMaxOutputTokens { limit }) => {
                    tracing::info!(
                        session_id = %session_id,
                        limit = %limit,
                        "SessionTask: updating max output tokens limit"
                    );
                    agent_loop.update_max_output_tokens_limit(limit);
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
                Some(SessionMessage::UpdateActiveTools { tool_definitions }) => {
                    tracing::info!(
                        session_id = %session_id,
                        tool_count = tool_definitions.len(),
                        "SessionTask: updating active tools"
                    );
                    context_builder.set_tool_definitions(tool_definitions);
                }
                Some(SessionMessage::UpdateSessionTitle { title }) => {
                    tracing::info!(
                        session_id = %session_id,
                        title = %title,
                        "SessionTask: updating session title"
                    );
                    let _ = agent_loop.update_session_title(&title);
                }
                Some(SessionMessage::Interrupt { reason }) => {
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "SessionTask: forwarding interrupt signal"
                    );
                    let _ = agent_inbound_tx.send(crate::agent::inbound::InboundMessage::Interrupt { reason }).await;
                }
                Some(SessionMessage::Stop) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: Stop received, shutting down"
                    );
                    break;
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

/// Apply any pending debug rewind.
///
/// Consumes `rewind_target` from the `DebugController` and truncates
/// the agent loop's history to the message count recorded in the
/// matching conversation snapshot.  Resets the iteration counter to
/// the target so execution resumes from the correct point.
///
/// This is the **single** entry point for rewind consumption.
/// It is called from:
/// - `SessionTask::run()` (via `tokio::select!` on rewind notify)
/// - `apply_debug_rewind_and_patches` (before each `agent_loop.run()`)
/// - `AgentLoop::await_debug_resume` (during pause polling)
///
/// The function is idempotent: calling it when no rewind is pending
/// is a no-op.
pub(crate) async fn apply_debug_rewind(
    debug_ctrl: &Arc<tokio::sync::Mutex<DebugController>>,
    session_id: &str,
    agent_loop: &mut AgentLoop,
) {
    let mut ctrl = debug_ctrl.lock().await;
    apply_debug_rewind_locked(&mut ctrl, session_id, &mut agent_loop.session.history);
}

/// Core rewind logic — assumes the caller already holds the controller lock.
///
/// Extracted into a lock-free helper so that `apply_debug_rewind_and_patches`
/// can apply rewind + patches + re-execute within a single lock acquisition,
/// eliminating the double-lock race window.
pub(crate) fn apply_debug_rewind_locked(
    ctrl: &mut DebugController,
    session_id: &str,
    history: &mut crate::agent::history::HistoryManager,
) {
    if let Some(target_iter) = ctrl.take_rewind_target() {
        // Find the message_count from the conversation snapshot
        // at the target iteration.
        let msg_count = ctrl
            .conversation_snapshots
            .iter()
            .find(|s| s.iteration == target_iter)
            .map(|s| s.message_count);

        if let Some(count) = msg_count {
            history.truncate_to(count);
            tracing::info!(
                session_id = %session_id,
                target_iteration = target_iter,
                messages_trimmed_to = count,
                "Debug rewind: history truncated"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                target_iteration = target_iter,
                "Debug rewind: no snapshot found for target iteration, history unchanged"
            );
        }

        // Reset iteration counter to the target so the agent loop
        // resumes from the correct point.
        ctrl.iteration = target_iter;
        tracing::debug!(
            session_id = %session_id,
            target_iteration = target_iter,
            "Debug: rewind applied, iteration reset"
        );
    }
}

/// Apply any pending debug controller operations (patches, re-execute)
/// before the next agent loop invocation.
///
/// This is called before each `agent_loop.run()` when DevMode is active.
/// It applies pending `PatchSet` to the `ContextBuilder` and consumes the
/// `re_execute_pending` flag.  Rewind is consumed separately via
/// `apply_debug_rewind` (called both here and from the select! loop).
async fn apply_debug_rewind_and_patches(
    debug_ctrl: &Option<Arc<tokio::sync::Mutex<DebugController>>>,
    session_id: &str,
    agent_loop: &mut AgentLoop,
    context_builder: &mut ContextBuilder,
) {
    let Some(debug_ctrl) = debug_ctrl else {
        return; // Production mode, no debug controller
    };

    // Single lock acquisition: apply rewind, patches, and re-execute
    // within the same critical section to avoid race windows between
    // successive lock/unlock cycles.
    let mut ctrl = debug_ctrl.lock().await;

    // ── Handle rewind ──
    apply_debug_rewind_locked(&mut ctrl, session_id, &mut agent_loop.session.history);

    // ── Apply pending patches (consume them) ──
    if let Some(patches) = ctrl.pending_patches.take() {
        context_builder.apply_patches(&patches);
        tracing::info!(
            session_id = %session_id,
            "Debug: pending patches applied to context builder and consumed"
        );
    }

    // ── Consume re-execute pending flag ──
    if ctrl.take_re_execute_pending() {
        tracing::info!(
            session_id = %session_id,
            "Debug: re-execute flag consumed, agent will process next message with updated context"
        );
    }
}
