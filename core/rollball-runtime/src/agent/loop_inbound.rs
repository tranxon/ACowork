//! Inbound message handling for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 3).
//! Contains all methods related to processing inbound messages:
//! - `apply_user_op`: central dispatch for UserOp variants
//! - `poll_stop`: non-blocking stop-signal check with message buffering
//! - `drain_inbound_queue`: full inbound queue drain with message injection
//!
//! D1 Deduplication: the message-into-history injection logic appeared 3 times
//! (deferred UserMessage/SystemNotification/IntentMessage, live ditto, and
//! run_inner iteration-limit pause). The `inject_inbound_into_history()` helper
//! consolidates these into a single implementation.

use rollball_core::providers::traits::{ChatMessage, MessageRole};

use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, IterationResult};
use crate::agent::loop_session::strip_think_block;
use crate::agent::session_state::SessionStatus;

// ── D1 Deduplication: shared message injection helper ──

/// Inject an inbound message into the history as a ChatMessage.
///
/// Used by `drain_inbound_queue` (deferred + live paths) and by
/// `run_inner` (iteration-limit pause re-injection). Returns `true`
/// if the message was injected (user/system/intent), `false` if it
/// was a control message that doesn't go into history.
pub(super) fn inject_inbound_into_history(
    msg: InboundMessage,
    history: &mut crate::agent::history::HistoryManager,
) -> bool {
    match msg {
        InboundMessage::UserMessage(text) => {
            tracing::info!(
                text_preview = %text.chars().take(80).collect::<String>(),
                "inject_inbound_into_history: injecting UserMessage"
            );
            history.append(ChatMessage::user(text));
            true
        }
        InboundMessage::SystemNotification { notification_type, data } => {
            tracing::info!("inject_inbound_into_history: system notification: {} = {:?}", notification_type, data);
            // Changed from System to User — MiniMax API rejects non-first system messages
            history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("[system:{}] {}", notification_type, data),
                name: Some("system".to_string()),
                ..Default::default()
            });
            true
        }
        InboundMessage::IntentMessage { from, action, params } => {
            tracing::info!("inject_inbound_into_history: intent from {}: {} params={:?}", from, action, params);
            history.append(ChatMessage::user(
                format!("[intent:{}:{}] {}", from, action, params),
            ));
            true
        }
        // Control messages are NOT injected into history
        _ => false,
    }
}

impl AgentLoop {
    /// Apply a user operation delivered via the `send_inbound()` fast channel.
    ///
    /// This is the central dispatch point for all `UserOp` variants received
    /// through `InboundMessage::UserOperation`. It handles operations that
    /// must take effect immediately even while the agent loop is mid-execution.
    ///
    /// Returns `true` if the operation is an interrupt (caller should abort
    /// the current loop).
    pub(crate) fn apply_user_op(&mut self, op: &crate::agent::inbound::UserOp) -> bool {
        match op {
            crate::agent::inbound::UserOp::StopLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: stop loop");
                true
            }
            crate::agent::inbound::UserOp::ContinueLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: continue loop (no-op here; handled at iteration limit pause)");
                false
            }
            crate::agent::inbound::UserOp::ApprovalDecision { .. } => {
                tracing::debug!("UserOp: approval decision (no-op here; handled via approval subsystem)");
                false
            }
            crate::agent::inbound::UserOp::QuestionAnswer { .. } => {
                tracing::debug!("UserOp: question answer (no-op here; handled via ask_user_question subsystem)");
                false
            }
            crate::agent::inbound::UserOp::UpdateRuntimeConfig {
                max_output_tokens,
                max_iterations,
                temperature,
                system_prompt_override,
                shell_approval_threshold,
            } => {
                tracing::info!(
                    max_output_tokens,
                    max_iterations,
                    temperature,
                    system_prompt_override = system_prompt_override.as_deref(),
                    shell_approval_threshold = shell_approval_threshold.as_deref(),
                    "UserOp: applying runtime config immediately in AgentLoop"
                );
                self.apply_runtime_config(
                    *max_output_tokens,
                    *max_iterations,
                    *temperature,
                    system_prompt_override.clone(),
                    shell_approval_threshold.clone(),
                );
                false
            }
        }
    }

    /// Non-blocking check for stop signals.
    ///
    /// Drains `inbound_rx` looking for `Stop` messages. Non-stop messages
    /// are buffered into `session.deferred_inbound` for later re-injection
    /// by `drain_inbound_queue()`. This preserves user messages that
    /// arrive while we're checking for stops (the "Stop to queue" UX).
    pub(crate) fn poll_stop(&mut self) -> bool {
        let mut should_stop = false;
        while let Ok(msg) = self.inbound_rx.try_recv() {
            match msg {
                InboundMessage::Stop { .. } => {
                    should_stop = true;
                    // Consume and continue — drain all pending stops
                }
                InboundMessage::UserOperation(op) => {
                    match &op {
                        crate::agent::inbound::UserOp::StopLoop { .. } => {
                            should_stop = true;
                            // Consume and continue — drain all pending stops
                        }
                        _ => {
                            // Buffer non-Stop UserOp for re-injection
                            // by drain_inbound_queue().
                            tracing::info!(
                                op = ?std::mem::discriminant(&op),
                                "poll_stop(): buffering UserOp for re-injection by drain_inbound_queue()"
                            );
                            self.session.deferred_inbound.push(InboundMessage::UserOperation(op));
                        }
                    }
                }
                other => {
                    // Buffer non-Stop messages for re-injection at the
                    // next drain_inbound_queue() call.
                    tracing::info!(
                        msg_type = ?std::mem::discriminant(&other),
                        "poll_stop(): buffering non-Stop message for re-injection by drain_inbound_queue()"
                    );
                    self.session.deferred_inbound.push(other);
                }
            }
        }
        should_stop
    }

    /// Drain inbound message queue (non-blocking).
    ///
    /// First processes any messages buffered by `poll_stop()` from
    /// the `deferred_inbound` stash, then drains the live channel.
    /// Injects external messages (user, system, intent) into history
    /// before each loop iteration. Applies size limits to prevent
    /// token explosion from oversized payloads.
    ///
    /// Returns `true` if at least one stop signal was found
    /// (the caller should stop the current agent loop).  ALL stop
    /// messages are consumed (not just the first one) to prevent
    /// residual stops from poisoning subsequent `run_inner()` calls.
    pub(crate) fn drain_inbound_queue(&mut self) -> bool {
        let mut should_stop = false;

        // ── Step 1: process messages deferred from poll_stop() ──
        // Collect to release the drain iterator's borrow on self.session
        // before calling apply_user_op() (which needs &mut self).
        let deferred: Vec<_> = self.session.deferred_inbound.drain(..).collect();
        for msg in deferred {
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::Stop { reason } => {
                    tracing::info!(reason = %reason, "Received deferred stop signal (consumed)");
                    should_stop = true;
                }
                InboundMessage::ContinueExecution { .. } => {
                    tracing::debug!("Ignoring deferred ContinueExecution");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    tracing::debug!("Ignoring deferred ApprovalDecision");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    tracing::debug!("Ignoring deferred QuestionAnswer");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing deferred UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        should_stop = true;
                    }
                }
                // D1 dedup: all injectable message types handled by helper
                other => {
                    inject_inbound_into_history(other, &mut self.session.history);
                }
            }
        }

        // ── Step 2: drain the live channel ──
        while let Ok(msg) = self.inbound_rx.try_recv() {
            // Enforce size limits before injecting
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::Stop { reason } => {
                    tracing::info!(reason = %reason, "Received stop signal (consumed)");
                    should_stop = true;
                    // Consume and continue — multiple stops may be queued
                    // from rapid Stop button clicks.  We must drain ALL of them
                    // so subsequent run_inner() calls aren't poisoned.
                }
                InboundMessage::ContinueExecution { .. } => {
                    // Continue is only meaningful during iteration limit pause;
                    // during normal drain, ignore it.
                    tracing::debug!("Ignoring ContinueExecution during normal drain");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    // Approval decisions are only meaningful during approval pause.
                    tracing::debug!("Ignoring ApprovalDecision during normal drain");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    // Question answers are only meaningful during question wait.
                    tracing::debug!("Ignoring QuestionAnswer during normal drain");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing live UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        should_stop = true;
                    }
                }
                // D1 dedup: all injectable message types handled by helper
                other => {
                    inject_inbound_into_history(other, &mut self.session.history);
                }
            }
        }
        should_stop
    }

    /// Handle agent stop (user clicked Stop or debug controller stopped).
    ///
    /// D3 deduplication: consolidates the nearly identical stopped-result
    /// construction from pre-tool stop and post-tool stop paths.
    /// Transitions to Idle, persists partial assistant response to JSONL,
    /// emits Stopped chunk event, and pushes debug step event.
    pub(crate) async fn handle_stopped(&mut self, content: &str) -> IterationResult {
        self.transition_status(SessionStatus::Idle);

        // Persist assistant message to JSONL conversation
        if let Some(ref conversation) = self.session.conversation {
            let assistant_text = strip_think_block(content);
            conversation.append_message("assistant", &assistant_text, None);
        }

        // Notify frontend via chunk channel
        let _ = self.core.try_send_chunk(ChunkEvent::Stopped {
            content: content.to_string(),
        });

        // Debug: push step event and auto-pause if stepping
        self.core.debug_observer.on_phase_step(
            crate::debug::protocol::DebugPhase::Idle,
            None,
            Some(serde_json::json!({"stopped": true, "content": content})),
        );
        self.core.debug_observer.on_phase_step_done().await;

        IterationResult::Stopped(content.to_string())
    }
}
