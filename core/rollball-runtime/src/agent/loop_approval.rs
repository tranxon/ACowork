//! Approval subsystem for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 2).
//! Contains all methods and types related to the tool approval flow:
//! - ApprovalDecision / ApprovalHandle types for the spawned-task → main-loop bridge
//! - await_approval_decision: blocks until user approves/rejects a tool
//! - await_question_answer: blocks until user answers an ask_user_question
//! - handle_approval_request: orchestrates the full approval lifecycle
//! - send_tool_approval_needed: emits the ChunkEvent to Gateway
//!
//! D4 Deduplication note: `await_approval_decision` and `await_question_answer`
//! share an isomorphic select! loop structure. Per ADR-014 Risk 4 guidance,
//! they are co-located in this file but NOT yet generalized into InboundWaiter.
//! The two methods differ in return type, match arm, and timeout semantics,
//! so premature abstraction would increase understanding cost.

use tokio::sync::{mpsc, oneshot};

use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session_state::SessionStatus;
use crate::security::approval_gate::ApprovalRequest;

/// Default approval/question timeout: 5 minutes.
///
/// D5 dedup: previously duplicated as `APPROVAL_TIMEOUT_SECS` (u64) in
/// `await_approval_decision` and `send_tool_approval_needed`, plus
/// `DEFAULT_TIMEOUT_SECS` (u32) in `await_question_answer`.
const APPROVAL_TIMEOUT_SECS: u64 = 300;

/// User's decision on a tool approval request.
#[derive(Debug, Clone)]
pub(crate) struct ApprovalDecision {
    pub approved: bool,
    #[allow(dead_code)]
    pub allow_all_session: bool,
    /// Human-readable reason for timeout or rejection (for LLM feedback)
    pub reason: Option<String>,
}

/// Lightweight handle for spawned tool tasks to request user approval.
///
/// The spawned task calls `request_approval()` (no timeout), which sends the
/// request to the AgentLoop main loop via an mpsc channel and blocks on a
/// oneshot. The main loop receives the request, emits ChunkEvent::ToolApprovalNeeded
/// to the Gateway (which forwards to the Desktop App), pauses via
/// `await_approval_decision()`, and resolves the oneshot when the user's
/// decision arrives as InboundMessage::ApprovalDecision.
#[derive(Clone)]
pub(crate) struct ApprovalHandle {
    pub(super) request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
}

impl ApprovalHandle {
    pub fn new(
        request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    ) -> Self {
        Self { request_tx }
    }

    /// Request user approval for a tool execution.
    /// Blocks without timeout until the user decides (Allow/Deny).
    pub async fn request_approval(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        if self.request_tx.send((req, tx)).await.is_err() {
            tracing::warn!("ApprovalHandle: request channel closed, auto-rejecting");
            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
        }
        rx.await.unwrap_or_else(|_| {
            tracing::warn!("ApprovalHandle: oneshot sender dropped, auto-rejecting");
            ApprovalDecision { approved: false, allow_all_session: false, reason: None }
        })
    }
}

impl AgentLoop {
    /// Wait for an `InboundMessage::ApprovalDecision` matching `request_id`.
    ///
    /// Non-matching messages are buffered in `session.deferred_inbound` for
    /// later processing. Also processes concurrent approval requests from
    /// `approval_rx` so that multiple tools needing approval don't deadlock.
    ///
    /// Returns `ApprovalDecision` with the user's choice, auto-rejects
    /// on Stop signal, channel close, or timeout (5 minutes).
    async fn await_approval_decision(&mut self, request_id: &str) -> ApprovalDecision {
        loop {
            tokio::select! {
                // Primary: wait for the matching approval decision from inbound channel
                msg = self.inbound_rx.recv() => {
                    match msg {
                        Some(InboundMessage::ApprovalDecision {
                            request_id: rid,
                            approved,
                            allow_all_session,
                            ..
                        }) if rid == request_id => {
                            tracing::info!(
                                request_id = %request_id,
                                approved,
                                allow_all_session,
                                "Approval decision received"
                            );
                            return ApprovalDecision { approved, allow_all_session, reason: None };
                        }
                        Some(InboundMessage::ApprovalDecision {
                            request_id: rid,
                            approved,
                            allow_all_session,
                            ..
                        }) => {
                            // Approval decision for a DIFFERENT request — buffer it.
                            // This can happen when multiple concurrent approval requests
                            // are in flight and responses arrive out of order.
                            tracing::debug!(
                                expected = %request_id,
                                got = %rid,
                                "Buffering approval decision for different request"
                            );
                            self.session.deferred_inbound.push(InboundMessage::ApprovalDecision {
                                request_id: rid,
                                approved,
                                allow_all_session,
                                reason: None,
                            });
                        }
                        Some(InboundMessage::Stop { reason }) => {
                            tracing::info!(
                                reason = %reason,
                                request_id = %request_id,
                                "Approval stopped, auto-rejecting"
                            );
                            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
                        }
                        Some(other) => {
                            tracing::debug!(
                                ?other,
                                "Buffering non-approval message during approval wait"
                            );
                            self.session.deferred_inbound.push(other);
                        }
                        None => {
                            tracing::warn!(
                                request_id = %request_id,
                                "Inbound channel closed during approval wait, auto-rejecting"
                            );
                            return ApprovalDecision { approved: false, allow_all_session: false, reason: None };
                        }
                    }
                }
                // Secondary: process concurrent approval requests from other spawned tasks.
                // Without this branch, a second tool needing approval would block on the
                // mpsc channel, its ToolApprovalNeeded event would never reach the Gateway,
                // and the user would never see the second approval dialog.
                approval_req = self.approval_rx.recv() => {
                    match approval_req {
                        Some((req, decision_tx)) => {
                            tracing::info!(
                                current_request_id = %request_id,
                                new_tool = %req.tool_name,
                                "Queuing concurrent approval request while waiting for decision"
                            );
                            self.handle_approval_request(req, decision_tx).await;
                            // After handling the concurrent request, continue waiting
                            // for the ORIGINAL request's decision.
                        }
                        None => {
                            tracing::warn!("Approval channel closed during approval wait");
                        }
                    }
                }
                // Timeout: auto-reject to prevent permanent deadlock when
                // a concurrent approval request is orphaned.
                _ = tokio::time::sleep(std::time::Duration::from_secs(APPROVAL_TIMEOUT_SECS)) => {
                    let reason_msg = format!("tool approval timed out after {}s", APPROVAL_TIMEOUT_SECS);
                    tracing::warn!(
                        request_id = %request_id,
                        timeout_secs = APPROVAL_TIMEOUT_SECS,
                        "Approval timed out, auto-rejecting"
                    );
                    return ApprovalDecision { approved: false, allow_all_session: false, reason: Some(reason_msg) };
                }
            }
        }
    }

    /// Wait for an `InboundMessage::QuestionAnswer` matching `request_id`.
    ///
    /// Non-matching messages are buffered in `session.deferred_inbound`.
    /// Also processes concurrent approval requests from `approval_rx`.
    /// Returns the user's answer string, or a cancellation/timeout message.
    pub(crate) async fn await_question_answer(&mut self, request_id: &str, timeout_seconds: Option<u32>) -> String {
        let timeout_duration = std::time::Duration::from_secs(
            timeout_seconds.unwrap_or(APPROVAL_TIMEOUT_SECS as u32) as u64
        );

        let timeout_future = tokio::time::timeout(timeout_duration, async {
            loop {
                tokio::select! {
                    msg = self.inbound_rx.recv() => {
                        match msg {
                        Some(InboundMessage::QuestionAnswer {
                            request_id: rid,
                            answer,
                        }) if rid == request_id => {
                            return answer;
                        }
                        Some(InboundMessage::QuestionAnswer {
                            request_id: rid,
                            answer,
                        }) => {
                            // Answer for a different question — buffer it
                            tracing::debug!(
                                expected = %request_id,
                                got = %rid,
                                "Buffering question answer for different request"
                            );
                            self.session.deferred_inbound.push(InboundMessage::QuestionAnswer {
                                request_id: rid,
                                answer,
                            });
                        }
                        Some(InboundMessage::Stop { reason }) => {
                            tracing::info!(
                                reason = %reason,
                                request_id = %request_id,
                                "Question wait stopped, returning cancelled"
                            );
                            return "[Cancelled: user stopped]".to_string();
                        }
                        Some(other) => {
                            tracing::debug!(
                                ?other,
                                "Buffering non-question message during question wait"
                            );
                            self.session.deferred_inbound.push(other);
                        }
                        None => {
                            tracing::warn!(
                                request_id = %request_id,
                                "Inbound channel closed during question wait, returning cancelled"
                            );
                            return "[Cancelled: channel closed]".to_string();
                        }
                    }
                }
                // Also process concurrent approval requests
                approval_req = self.approval_rx.recv() => {
                    match approval_req {
                        Some((req, decision_tx)) => {
                            tracing::info!(
                                "Queuing concurrent approval request while waiting for question answer"
                            );
                            self.handle_approval_request(req, decision_tx).await;
                        }
                        None => {
                            tracing::warn!("Approval channel closed during question wait");
                        }
                    }
                }
            }
        }
    });
        let result = timeout_future.await;

        match result {
            Ok(answer) => answer,
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %request_id,
                    timeout_secs = %timeout_seconds.unwrap_or(APPROVAL_TIMEOUT_SECS as u32),
                    "Question answer timed out"
                );
                "[Timeout: user did not respond]".to_string()
            }
        }
    }

    /// Send ToolApprovalNeeded chunk event to Gateway (via on_chunk channel).
    fn send_tool_approval_needed(&self, request_id: &str, req: &ApprovalRequest) {
        let _ = self.core.try_send_chunk(ChunkEvent::ToolApprovalNeeded {
            request_id: request_id.to_string(),
            tool_name: req.tool_name.clone(),
            action: req.action.clone(),
            risk_level: req.risk_level.label().to_string(),
            reason: req.reason.clone(),
            tool_call_id: req.tool_call_id.clone(),
            approval_timeout_secs: APPROVAL_TIMEOUT_SECS,
        });
    }

    /// Handle an approval request received on the approval_rx channel.
    ///
    /// Called from `execute_tools_parallel`'s `select!` when a spawned tool
    /// task sends an approval request. Generates a unique request ID, sends
    /// `ChunkEvent::ToolApprovalNeeded` to the Gateway, blocks the main loop
    /// via `await_approval_decision()`, and resolves the spawned task's
    /// oneshot with the user's decision.
    pub(crate) fn handle_approval_request(
        &mut self,
        req: ApprovalRequest,
        decision_tx: oneshot::Sender<ApprovalDecision>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let request_id = self
                .approval_next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .to_string();

            tracing::info!(
                request_id = %request_id,
                tool_name = %req.tool_name,
                action = %req.action,
                risk = %req.risk_level.label(),
                "Handling approval request from spawned tool task"
            );

            // 1. Send ChunkEvent to Gateway → Desktop App
            self.send_tool_approval_needed(&request_id, &req);

            // ADR-014: Streaming → WaitingApproval
            self.transition_status(SessionStatus::WaitingApproval {
                request_id: request_id.clone(),
            });

            // 2. Pause and wait for user decision (no timeout)
            let decision = self.await_approval_decision(&request_id).await;

            // ADR-014: WaitingApproval → Streaming (resume after approval/rejection)
            self.transition_status(SessionStatus::Streaming { message_id: None });

            // 3. Resolve the spawned task's oneshot
            let _ = decision_tx.send(decision);
        })
    }
}
