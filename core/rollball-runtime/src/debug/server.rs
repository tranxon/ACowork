//! Debug Protocol WebSocket server.
//!
//! Listens on `ws://127.0.0.1:19878` when Agent Runtime starts in DevMode.
//! Provides a JSON-RPC 2.0 endpoint for the Desktop App's debug panel.
//!
//! ## Architecture
//! - **Single client**: only one Desktop App can connect at a time.
//! - **Shared state**: `Arc<Mutex<DebugController>>` shared between
//!   the server task and the AgentLoop.
//! - **Event channel**: `mpsc::UnboundedSender<Event>` for pushing
//!   notifications from AgentLoop to WebSocket client.
//!
//! ## Lifecycle
//! 1. `DebugProtocolServer::start()` is called when `--dev-mode` is set.
//! 2. A tokio task is spawned to listen for WebSocket connections.
//! 3. On connection, the task enters a read-write loop:
//!    - Read: parse JSON-RPC requests, route to handler, send response.
//!    - Write: forward events from AgentLoop as notifications.
//! 4. On disconnect, the server returns to listening state.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream::StreamExt;
use futures::SinkExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::controller::DebugController;
use super::controller::DebugState;
use super::protocol::{
    self, DebugPhase, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};

// ── Event Bus ─────────────────────────────────────────────────────────

/// Events that AgentLoop can push to the debug server.
#[derive(Debug, Clone)]
pub enum DebugEvent {
    /// Agent execution state changed (paused, resumed, etc.)
    StateChanged {
        old_phase: DebugPhase,
        new_phase: DebugPhase,
        iteration: u32,
    },
    /// A conversation step completed
    Step {
        iteration: u32,
        phase: DebugPhase,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
        usage: Option<protocol::DebugUsage>,
    },
    /// Context was built for an iteration
    ContextBuilt {
        iteration: u32,
        sections: protocol::ContextSections,
        total_token_estimate: usize,
    },
    /// Execution state changed (Running/Paused/Stepping/Stopped)
    ExecutionStateChanged {
        new_state: DebugState,
        iteration: u32,
    },
}

/// Internal wrapper that tags an event with its originating session ID.
struct TaggedEvent {
    session_id: String,
    event: DebugEvent,
}

/// Handle for sending events to the WebSocket client.
///
/// Each session gets its own `DebugEventSender` with the session's ID
/// embedded, so events are automatically tagged at send time.
/// Clone is cheap — multiple senders can push events concurrently.
#[derive(Debug, Clone)]
pub struct DebugEventSender {
    tx: mpsc::UnboundedSender<TaggedEvent>,
    session_id: String,
}

impl DebugEventSender {
    /// Return the session ID that this sender tags events with.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a debug event to the connected WebSocket client.
    /// Returns `true` if the event was queued, `false` if the channel is closed.
    pub fn send(&self, event: DebugEvent) -> bool {
        self.tx
            .send(TaggedEvent {
                session_id: self.session_id.clone(),
                event,
            })
            .is_ok()
    }

    /// Check if the event channel is still open.
    pub fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Create a sender for a specific session, sharing the same underlying channel.
    pub fn for_session(&self, session_id: String) -> Self {
        Self {
            tx: self.tx.clone(),
            session_id,
        }
    }
}

// ── Server ────────────────────────────────────────────────────────────

/// Debug Protocol WebSocket server state.
///
/// Per-session debug isolation: each session has its own `DebugController`
/// (iteration counter, state, snapshots). The frontend sends
/// `session_id` in every request; the server routes to the correct
/// controller without needing a server-side "current session".
pub struct DebugProtocolServer {
    /// Per-session debug controllers, shared with SessionManager for
    /// dynamic add/remove as sessions are created/destroyed.
    sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<Mutex<DebugController>>>>>,
    /// Event sender (clone this and call `for_session()` for per-session senders)
    event_tx: mpsc::UnboundedSender<TaggedEvent>,
    /// Event receiver (used by server task to forward to WebSocket)
    event_rx: mpsc::UnboundedReceiver<TaggedEvent>,
    /// Port to bind the WebSocket server to
    port: u16,
    /// Track which session was last targeted by a control command
    /// (step/resume/pause) so that getState without an explicit
    /// session_id can fall back to the actively-debugged session.
    last_active_session_id: Option<String>,
}

impl DebugProtocolServer {
    /// Create a new DebugProtocolServer with shared per-session state.
    ///
    /// `port` is the TCP port to bind the WebSocket server to.
    /// `sessions` is shared with SessionManager for dynamic add/remove.
    pub fn new(
        port: u16,
        sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<Mutex<DebugController>>>>>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            sessions,
            event_tx,
            event_rx,
            port,
            last_active_session_id: None,
        }
    }

    /// Start the debug protocol server in a background task.
    ///
    /// Binds to `ws://127.0.0.1:19878` and spawns a tokio task
    /// to accept and handle WebSocket connections.
    ///
    /// Returns a `DebugEventSender` template — clone it and call
    /// `for_session(session_id)` to create per-session senders.
    pub async fn start(self) -> DebugEventSender {
        let template = DebugEventSender {
            tx: self.event_tx.clone(),
            session_id: String::new(),
        };

        tokio::spawn(async move {
            self.run().await;
        });

        template
    }

    /// Main server loop: listen, accept, handle, repeat.
    async fn run(mut self) {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));

        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                tracing::warn!(
                    error = %e,
                    addr = %addr,
                    "DebugProtocolServer: port in use, attempting to free it by killing old process"
                );
                kill_process_on_port(addr.port()).await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                match TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e2) => {
                        tracing::error!(
                            error = %e2,
                            addr = %addr,
                            "DebugProtocolServer: failed to bind after killing old process, debug protocol unavailable"
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    addr = %addr,
                    "DebugProtocolServer: failed to bind, debug protocol unavailable"
                );
                return;
            }
        };

        tracing::info!(
            addr = %addr,
            "DebugProtocolServer: listening for connections"
        );

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    tracing::info!(
                        peer = %peer_addr,
                        "DebugProtocolServer: client connected"
                    );
                    self.handle_connection(stream, peer_addr).await;
                    tracing::info!(
                        peer = %peer_addr,
                        "DebugProtocolServer: client disconnected"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "DebugProtocolServer: accept error"
                    );
                }
            }
        }
    }

    /// Handle a single WebSocket connection.
    async fn handle_connection(&mut self, stream: TcpStream, peer_addr: SocketAddr) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    peer = %peer_addr,
                    "DebugProtocolServer: WebSocket upgrade failed"
                );
                return;
            }
        };

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        loop {
            tokio::select! {
                // Read incoming JSON-RPC requests from WebSocket
                msg = ws_receiver.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            let response = self
                                .handle_json_rpc(&text)
                                .await;
                            if let Some(resp_text) = response {
                                tracing::info!(
                                    response = %resp_text,
                                    "DebugProtocolServer: sending JSON-RPC response"
                                );
                                if let Err(e) = ws_sender
                                    .send(Message::Text(resp_text.into()))
                                    .await
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "DebugProtocolServer: failed to send response"
                                    );
                                    break;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            tracing::info!("DebugProtocolServer: client sent close frame");
                            break;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            // Respond to keep-alive pings
                            let _ = ws_sender.send(Message::Pong(data)).await;
                        }
                        Some(Ok(_)) => {
                            // Ignore binary and other messages
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                "DebugProtocolServer: WebSocket read error"
                            );
                            break;
                        }
                        None => {
                            tracing::info!("DebugProtocolServer: WebSocket stream ended");
                            break;
                        }
                    }
                }

                // Forward events from AgentLoop to WebSocket client.
                // ALL session events are forwarded; the frontend routes
                // them by session_id via its per-session debugStore.
                tagged = self.event_rx.recv() => {
                    match tagged {
                        Some(TaggedEvent { session_id, event: debug_event }) => {
                            let method = event_method_name(&debug_event);
                            let notification = event_to_notification(debug_event, &session_id);
                            match serde_json::to_string(&notification) {
                                Ok(json) => {
                                    tracing::info!(method = %method, session_id = %session_id, "DebugProtocolServer: forwarding event to client");
                                    if let Err(e) = ws_sender
                                        .send(Message::Text(json.into()))
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "DebugProtocolServer: failed to send event"
                                        );
                                        break;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "DebugProtocolServer: failed to serialize event"
                                    );
                                }
                            }
                        }
                        None => {
                            // Event channel closed
                            break;
                        }
                    }
                }
            }
        }

        // On disconnect, reset all non-Stopped session controllers to Running.
        // If the user explicitly stopped debugging, we must not auto-resume
        // the agent loop when the WebSocket closes.
        for ctrl_arc in self.sessions.read().await.values() {
            let mut ctrl = ctrl_arc.lock().await;
            if ctrl.state != super::controller::DebugState::Stopped {
                ctrl.state = super::controller::DebugState::Running;
                ctrl.phase = DebugPhase::Idle;
            }
        }
    }

    /// Handle an incoming JSON-RPC request.
    ///
    /// Returns an optional JSON response string. `None` means no response
    /// is needed (e.g., for invalid JSON that can't be parsed at all).
    async fn handle_json_rpc(&mut self, text: &str) -> Option<String> {
        let request: JsonRpcRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    raw = %text,
                    "DebugProtocolServer: failed to parse JSON-RPC request"
                );
                let resp = JsonRpcResponse::error(
                    serde_json::Value::Null,
                    protocol::PARSE_ERROR,
                    format!("Parse error: {e}"),
                    None,
                );
                return Some(serde_json::to_string(&resp).unwrap_or_default());
            }
        };

        tracing::info!(
            method = %request.method,
            id = %request.id,
            "DebugProtocolServer: received JSON-RPC request"
        );

        let result = self.route_method(&request.method, &request.params, request.id.clone()).await;
        let response = result.unwrap_or_else(|e| {
            JsonRpcResponse::error(
                request.id.clone(),
                e.code,
                e.message,
                e.data,
            )
        });

        Some(serde_json::to_string(&response).unwrap_or_default())
    }

    /// Route a JSON-RPC method to its handler.
    async fn route_method(
        &mut self,
        method: &str,
        params: &serde_json::Value,
        id: serde_json::Value,
    ) -> Result<JsonRpcResponse, MethodError> {
        // Resolve session from request params. Priority:
        // 1. Explicit `session_id` in params (sent by frontend)
        // 2. Last session targeted by a control command (step/resume/pause)
        // 3. First session in the map (best-effort fallback)
        let explicit_session_id = params
            .get("session_id")
            .and_then(|v| v.as_str());
        let session_id = explicit_session_id
            .map(|s| s.to_string())
            .or_else(|| self.last_active_session_id.clone())
            .or_else(|| {
                self.sessions
                    .try_read()
                    .ok()
                    .and_then(|guard| guard.keys().next().cloned())
            })
            .ok_or_else(|| {
                MethodError::new(
                    -32000,
                    "No debug session available — create a session first".to_string(),
                )
            })?;
        tracing::info!(
            method = %method,
            explicit_session_id = ?explicit_session_id,
            resolved_session_id = %session_id,
            "[DBG-TRACE] route_method: session resolution"
        );
        let ctrl_arc = self
            .sessions
            .read()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                MethodError::new(
                    -32000,
                    format!("No debug session found for session_id: {session_id}"),
                )
            })?;
        let mut ctrl = ctrl_arc.lock().await;

        // Helper to send a tagged event for the current session.
        let send_event = |event_tx: &mpsc::UnboundedSender<TaggedEvent>,
                          sid: &str,
                          event: DebugEvent| {
            let _ = event_tx.send(TaggedEvent {
                session_id: sid.to_string(),
                event,
            });
        };

        match method {
            // ── Execution Control ──
            "debugger.resume" => {
                self.last_active_session_id = Some(session_id.clone());
                ctrl.state = DebugState::Running;
                let iteration = ctrl.iteration;
                // event_tx.send() is non-blocking (unbounded channel) —
                // safe to call while holding the controller lock at RPC
                // route level (agent loop acquires the lock separately).
                send_event(
                    &self.event_tx,
                    &session_id,
                    DebugEvent::ExecutionStateChanged {
                        new_state: DebugState::Running,
                        iteration,
                    },
                );
                // Wake up the SessionTask so it can re-run the agent loop
                // if it has already exited (e.g. after rewind was issued
                // post-completion).  This is a no-op if the agent loop is
                // already polling in await_debug_resume.
                ctrl.resume_notify.notify_one();
                tracing::info!("Debug: resume — agent loop will continue");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({}),
                ))
            }

            "debugger.pause" => {
                self.last_active_session_id = Some(session_id.clone());
                ctrl.state = DebugState::Paused;
                let iteration = ctrl.iteration;
                send_event(
                    &self.event_tx,
                    &session_id,
                    DebugEvent::ExecutionStateChanged {
                        new_state: DebugState::Paused,
                        iteration,
                    },
                );
                tracing::info!("Debug: pause — agent loop will pause at next check");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({}),
                ))
            }

            "debugger.step" => {
                self.last_active_session_id = Some(session_id.clone());
                ctrl.state = DebugState::Stepping;
                let iteration = ctrl.iteration;
                send_event(
                    &self.event_tx,
                    &session_id,
                    DebugEvent::ExecutionStateChanged {
                        new_state: DebugState::Stepping,
                        iteration,
                    },
                );
                // Wake the SessionTask so it can re-enter the agent loop.
                // await_debug_resume() inside execute_single_iteration also
                // waits on this notify — this covers both the running and
                // idle-session cases.
                ctrl.resume_notify.notify_one();
                tracing::info!("Debug: step — agent loop will execute one step");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({}),
                ))
            }

            "debugger.stop" => {
                self.last_active_session_id = Some(session_id.clone());
                ctrl.state = DebugState::Stopped;
                let iteration = ctrl.iteration;
                send_event(
                    &self.event_tx,
                    &session_id,
                    DebugEvent::ExecutionStateChanged {
                        new_state: DebugState::Stopped,
                        iteration,
                    },
                );
                tracing::info!("Debug: stop — agent loop terminated");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({}),
                ))
            }

            // ── State Query ──
            "debugger.getState" => {
                let ctrl_ptr = Arc::as_ptr(&ctrl_arc) as *const ();
                let current_state = ctrl.state;
                let state = protocol::GetStateResult {
                    iteration: ctrl.iteration,
                    phase: ctrl.phase,
                    messages: Vec::new(), // TODO: populate in S2.3 with actual messages
                    snapshot_ids: ctrl
                        .conversation_snapshots
                        .iter()
                        .map(|s| s.id.clone())
                        .collect(),
                    usage: protocol::DebugUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    },
                    paused: current_state == DebugState::Paused,
                    state: serde_json::to_string(&current_state)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string(),
                };
                let result = serde_json::to_value(state)
                    .map_err(|e| MethodError::internal(e.to_string()))?;
                tracing::info!(
                    session_id = %session_id,
                    ctrl_ptr = ?ctrl_ptr,
                    iteration = ctrl.iteration,
                    dbg_state = %serde_json::to_string(&current_state).unwrap_or_default().trim_matches('"'),
                    "[DBG-TRACE] Debug: getState response"
                );
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    result,
                ))
            }

            // ── Context Snapshots (S2.4 will flesh these out) ──
            "debugger.getContextSnapshot" => {
                let snap_params: protocol::GetContextSnapshotParams = serde_json::from_value(params.clone())
                    .map_err(|e| MethodError::invalid_params(e.to_string()))?;
                match ctrl.get_context_snapshot(snap_params.iteration) {
                    Some(snap) => {
                        let sections = protocol::ContextSections::from(&snap.sections);
                        let result = protocol::GetContextSnapshotResult {
                            iteration: snap.iteration,
                            built_at: snap.built_at.to_rfc3339(),
                            sections,
                            total_token_estimate: snap.total_token_estimate,
                            phase: DebugPhase::BuildContext,
                        };
                        let json = serde_json::to_value(result)
                            .map_err(|e| MethodError::internal(e.to_string()))?;
                        Ok(JsonRpcResponse::success(
                            id.clone(),
                            json,
                        ))
                    }
                    None => Err(MethodError::new(
                        -32002,
                        format!(
                            "No context snapshot for iteration {}",
                            snap_params.iteration
                        ),
                    )),
                }
            }

            "debugger.getSection" => {
                let sec_params: protocol::GetSectionParams = serde_json::from_value(params.clone())
                    .map_err(|e| MethodError::invalid_params(e.to_string()))?;
                tracing::info!(
                    iteration = sec_params.iteration,
                    section = %sec_params.section,
                    "Debug: getSection request"
                );
                match ctrl.get_context_snapshot(sec_params.iteration) {
                    Some(snap) => {
                        let section_content = match sec_params.section.as_str() {
                            "system_prompt" => &snap.sections.system_prompt,
                            "workspace_context" => &snap.sections.workspace_context,
                            "environment" => &snap.sections.environment,
                            "tool_definitions" => &snap.sections.tool_definitions,
                            "skill_instructions" => &snap.sections.skill_instructions,
                            "retrieved_memory" => &snap.sections.retrieved_memory,
                            "identity_context" => &snap.sections.identity_context,
                            _ => {
                                return Err(MethodError::invalid_params(format!(
                                    "Unknown section: {}",
                                    sec_params.section
                                )));
                            }
                        };
                        let result = protocol::GetSectionResult {
                            content: section_content.content.clone(),
                            hash: section_content.hash.clone(),
                            token_count: section_content.token_estimate,
                        };
                        let json = serde_json::to_value(result)
                            .map_err(|e| MethodError::internal(e.to_string()))?;
                        tracing::info!(
                            iteration = sec_params.iteration,
                            section = %sec_params.section,
                            content_len = section_content.content.len(),
                            "Debug: getSection returning result"
                        );
                        Ok(JsonRpcResponse::success(
                            id.clone(),
                            json,
                        ))
                    }
                    None => {
                        tracing::warn!(
                            iteration = sec_params.iteration,
                            "Debug: getSection — no context snapshot found"
                        );
                        Err(MethodError::new(
                            -32002,
                            format!(
                                "No context snapshot for iteration {}",
                                sec_params.iteration
                            ),
                        ))
                    }
                }
            }

            // ── Context Editing ──
            "debugger.rewind" => {
                let rw_params: protocol::RewindParams = serde_json::from_value(params.clone())
                    .map_err(|e| MethodError::invalid_params(e.to_string()))?;
                let target = rw_params.to_iteration;

                // When rewind is invoked while Stopped, transition back to
                // Paused.  Rewind is an explicit user action signalling
                // intent to continue working from a previous iteration.
                // Without this transition, await_debug_resume() returns
                // false immediately and agent_loop.run() short-circuits
                // with "Agent stopped by debugger", making rewind
                // effectively useless after Stop.
                let was_stopped = ctrl.state == DebugState::Stopped;
                if was_stopped {
                    ctrl.state = DebugState::Paused;
                }

                // Reset iteration counter immediately so that getState
                // and any other consumers see the correct value without
                // waiting for the SessionTask to consume rewind_target.
                ctrl.iteration = target;

                // Store rewind target for consumer to apply
                ctrl.rewind_target = Some(target);
                // Notify consumers (await_debug_resume + SessionTask)
                // that a rewind is pending, eliminating the need for
                // polling.
                ctrl.notify_rewind();
                // Clear any pending patches — rewind supersedes patches
                ctrl.pending_patches = None;
                // Truncate snapshots after the target iteration
                ctrl.truncate_snapshots_after(target);

                // Find the message_count from the matching snapshot
                let message_count = ctrl
                    .conversation_snapshots
                    .iter()
                    .find(|s| s.iteration == target)
                    .map(|s| s.message_count)
                    .unwrap_or(0);

                tracing::info!(
                    target_iteration = target,
                    message_count,
                    was_stopped,
                    "Debug: rewind — history will be truncated, patches cleared"
                );

                // If state transitioned from Stopped → Paused, push
                // an ExecutionStateChanged event so the frontend's debug
                // panel updates to show "Paused" instead of "Stopped".
                if was_stopped {
                    send_event(
                        &self.event_tx,
                        &session_id,
                        DebugEvent::ExecutionStateChanged {
                            new_state: DebugState::Paused,
                            iteration: target,
                        },
                    );
                }

                let result = serde_json::to_value(protocol::RewindResult {
                    rewound_to_iteration: target,
                    messages_trimmed_to: message_count,
                })
                .map_err(|e| MethodError::internal(e.to_string()))?;

                Ok(JsonRpcResponse::success(
                    id.clone(),
                    result,
                ))
            }

            "debugger.patchContext" => {
                let pc_params: protocol::PatchContextParams = serde_json::from_value(params.clone())
                    .map_err(|e| MethodError::invalid_params(e.to_string()))?;

                // Bug 2 fix: merge incrementally instead of replacing
                let merged_patches = match ctrl.pending_patches.take() {
                    Some(existing) => {
                        let mut merged = existing;
                        merged.merge(pc_params.patches);
                        merged
                    }
                    None => pc_params.patches,
                };

                // Bug 3 fix: reflect patches in the context snapshot so
                // getSection returns the patched content, not the original.
                // merged_patches is owned (not borrowed from ctrl), so no borrow conflict.
                let current_iter = ctrl.iteration;
                // Use the model stored by capture_context_snapshot for
                // model-aware token counting via the unified API.
                // Clone before get_mut() to avoid borrow conflict.
                let model_owned = ctrl.current_model.clone().unwrap_or_default();
                let model: &str = &model_owned;
                if let Some(snap) = ctrl.context_snapshots.get_mut(&current_iter) {
                    if let Some(ref prompt) = merged_patches.system_prompt {
                        snap.sections.system_prompt =
                            super::controller::SectionContent::new(prompt.clone(), model);
                    }
                    if let Some(ref tools) = merged_patches.tool_definitions {
                        let content = serde_json::to_string_pretty(tools)
                            .unwrap_or_else(|_| serde_json::to_string(tools).unwrap_or_default());
                        snap.sections.tool_definitions =
                            super::controller::SectionContent::new(content, model);
                    }
                    if let Some(ref skills) = merged_patches.skill_instructions {
                        snap.sections.skill_instructions =
                            super::controller::SectionContent::new(skills.clone(), model);
                    }
                    if let Some(ref memory) = merged_patches.retrieved_memory {
                        let content = memory.to_string();
                        snap.sections.retrieved_memory =
                            super::controller::SectionContent::new(content, model);
                    }
                    if let Some(ref identity) = merged_patches.identity_context {
                        let content = identity.to_string();
                        snap.sections.identity_context =
                            super::controller::SectionContent::new(content, model);
                    }
                    if let Some(ref workspace) = merged_patches.workspace_context {
                        snap.sections.workspace_context =
                            super::controller::SectionContent::new(workspace.clone(), model);
                    }
                    if let Some(ref env) = merged_patches.environment {
                        // Empty string clears the override — build() falls back
                        // to auto-detect.  The snapshot must match this behavior.
                        let content = if env.is_empty() {
                            crate::agent::context::detect_environment_text()
                        } else {
                            env.clone()
                        };
                        snap.sections.environment =
                            super::controller::SectionContent::new(content, model);
                    }
                    tracing::info!(
                        iteration = current_iter,
                        "Debug: context snapshot updated with patched content"
                    );
                } else {
                    tracing::warn!(
                        iteration = current_iter,
                        "Debug: patchContext — no context snapshot to update"
                    );
                }

                ctrl.pending_patches = Some(merged_patches);

                tracing::info!("Debug: context patches merged and stored for next reExecute");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({}),
                ))
            }

            "debugger.reExecute" => {
                // Set re-execute pending flag for SessionTask to consume
                ctrl.set_re_execute_pending();
                // Set state to Running so the agent loop can proceed
                ctrl.state = super::controller::DebugState::Running;
                tracing::info!("Debug: reExecute — pending flag set, execution will proceed with patches (if any)");
                Ok(JsonRpcResponse::success(
                    id.clone(),
                    serde_json::json!({ "has_patches": ctrl.pending_patches.is_some() }),
                ))
            }

            _ => Err(MethodError::new(
                protocol::METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            )),
        }
    }
}

/// Get the event method name for logging.
fn event_method_name(event: &DebugEvent) -> &'static str {
    match event {
        DebugEvent::StateChanged { .. } => "debugger.onStateChange",
        DebugEvent::Step { .. } => "debugger.onStep",
        DebugEvent::ContextBuilt { .. } => "debugger.onContextBuilt",
        DebugEvent::ExecutionStateChanged { .. } => "debugger.onExecutionStateChange",
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Convert a DebugEvent to a JSON-RPC notification, tagging it with the
/// originating session ID so the frontend can filter events per session.
fn event_to_notification(event: DebugEvent, session_id: &str) -> JsonRpcNotification {
    match event {
        DebugEvent::StateChanged {
            old_phase,
            new_phase,
            iteration,
        } => {
            let params = serde_json::json!({
                "session_id": session_id,
                "old_phase": format!("{:?}", old_phase),
                "new_phase": format!("{:?}", new_phase),
                "iteration": iteration,
            });
            JsonRpcNotification::new("debugger.onStateChange", params)
        }
        DebugEvent::Step {
            iteration,
            phase,
            input,
            output,
            usage,
        } => {
            let params = serde_json::json!({
                "session_id": session_id,
                "iteration": iteration,
                "phase": format!("{:?}", phase),
                "input": input,
                "output": output,
                "usage": usage,
            });
            JsonRpcNotification::new("debugger.onStep", params)
        }
        DebugEvent::ContextBuilt {
            iteration,
            sections,
            total_token_estimate,
        } => {
            let params = serde_json::json!({
                "session_id": session_id,
                "iteration": iteration,
                "sections": sections,
                "total_token_estimate": total_token_estimate,
            });
            JsonRpcNotification::new("debugger.onContextBuilt", params)
        }
        DebugEvent::ExecutionStateChanged {
            new_state,
            iteration,
        } => {
            let params = serde_json::json!({
                "session_id": session_id,
                "new_state": new_state,
                "iteration": iteration,
            });
            JsonRpcNotification::new("debugger.onExecutionStateChange", params)
        }
    }
}

// ── Method Routing Error ──────────────────────────────────────────────

/// Lightweight error type for method routing (not serialized directly).
struct MethodError {
    code: i32,
    message: String,
    data: Option<serde_json::Value>,
}

impl MethodError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(protocol::INTERNAL_ERROR, message)
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(protocol::INVALID_PARAMS, message)
    }
}

// ── Port Cleanup ──────────────────────────────────────────────────────

/// Kill the process holding a TCP port, so the debug server can rebind.
///
/// This handles the case where a previous runtime process was orphaned
/// (e.g. Gateway was killed without stopping the child) and still holds
/// the debug WebSocket port. Without this, the new runtime would fail to
/// bind and debug mode would be silently unavailable.
///
/// Platform behavior:
/// - **Windows**: uses `netstat -ano` to find the PID, then `taskkill /F`.
/// - **Unix**: uses `lsof -ti` (fallback: `fuser -k`) to find and kill.
async fn kill_process_on_port(port: u16) {
    tracing::info!(port, "Attempting to kill process holding debug port");

    #[cfg(windows)]
    {
        let port_filter = format!(":{}", port);
        match tokio::process::Command::new("cmd")
            .args(["/C", "netstat", "-ano"])
            .output()
            .await
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Find line containing the port, extract last column (PID)
                for line in stdout.lines() {
                    if line.contains(&port_filter) && line.contains("LISTENING") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if let Some(pid_str) = parts.last() {
                            if let Ok(pid) = pid_str.parse::<u32>() {
                                tracing::info!(pid, "Killing process holding debug port");
                                let _ = tokio::process::Command::new("taskkill")
                                    .args(["/F", "/PID", &pid.to_string()])
                                    .output()
                                    .await;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to run netstat for port cleanup");
            }
        }
    }

    #[cfg(not(windows))]
    {
        // Try lsof first (macOS + most Linux)
        let port_str = port.to_string();
        match tokio::process::Command::new("lsof")
            .args(["-ti", &format!(":{}", port_str)])
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if let Ok(pid) = trimmed.parse::<u32>() {
                        tracing::info!(pid, "Killing process holding debug port (lsof)");
                        let _ = tokio::process::Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .output()
                            .await;
                    }
                }
            }
            _ => {
                // Fallback: try fuser
                match tokio::process::Command::new("fuser")
                    .args(["-k", "-TERM", &format!("{}/tcp", port_str)])
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {
                        tracing::info!(port, "Killed process holding debug port (fuser)");
                    }
                    _ => {
                        tracing::warn!(port, "Could not identify process holding debug port");
                    }
                }
            }
        }
    }
}
