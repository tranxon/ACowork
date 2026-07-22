//! Tool execution module
//!
//! Extracted from [`super::loop_`] to provide reusable parallel tool execution
//! shared between production [`AgentLoop::run`] and debug `DebugSessionTask`.
//!
//! Handles:
//! - Tool activation filtering (done at registry build time — no runtime check needed)
//! - Parallel execution with per-tool timeout + iteration deadline
//! - Result collection with original ordering
//! - Tool call preparation (dedup, history, JSONL persist, chunk emit)
//! - Pre/post-execution loop detection
//! - Tool result persistence and emission

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use acowork_core::providers::traits::{ChatMessage, ToolCall};
use acowork_core::tools::traits::Tool;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::agent::context::ContextBuilder;
use crate::agent::loop_detector::{LoopDetectionResult, LoopPattern, ResponseLevel};
use crate::agent::session_state::SessionStatus;
use crate::error::{Result, RuntimeError};
use crate::security::approval_gate::{ApprovalGate, ApprovalRequest};
use crate::security::shell_risk::{self, ShellRisk};
use acowork_core::ShellApprovalThreshold;

use super::loop_::{AgentLoop, ControlDecision};
use super::loop_approval::ApprovalHandle;

impl AgentLoop {
    /// Execute tool calls in parallel with per-tool timeout and iteration-level deadline.
    ///
    /// Tool activation filtering is done at registry build time via
    /// [`ToolRegistry::activate`] — no runtime permission check needed here.
    ///
    /// Returns results in the same order as input tool calls.
    /// Individual tool failures are captured as error strings, not propagated.
    /// Returns (results, interrupt) — `interrupt` is `Some(ControlDecision)` when
    /// a control signal (Stop/Pause) was received during tool execution, telling
    /// the caller whether to exit or pause the loop.
    pub(crate) async fn execute_tools_parallel(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> (Vec<(String, bool)>, Option<ControlDecision>) {
        if tool_calls.is_empty() {
            return (Vec::new(), None);
        }

        tracing::info!(
            tool_calls_count = tool_calls.len(),
            tools = ?tool_calls
                .iter()
                .map(|t| &t.function.name)
                .collect::<Vec<_>>(),
            "Executing tool calls"
        );

        // Phase 1: Execute tools in parallel with spawn + select + deadline
        let all_indices: Vec<usize> = (0..tool_calls.len()).collect();

        let tool_timeout = Duration::from_millis(self.core.config.timeouts.tool_timeout_ms);
        let iteration_timeout = Duration::from_millis(self.core.config.timeouts.iteration_timeout_ms);

        // Channel to collect results from spawned tasks
        // Each item: (original_index, (content, is_transient))
        let (tx, mut rx) = mpsc::channel::<(usize, (String, bool))>(tool_calls.len());

        // Clone shared state for spawned tasks
        let approval_handle = self.approval_handle.clone();
        let approval_gate = self.core.approval_gate.clone();
        let shell_threshold = *self.core.shell_approval_threshold();
        let use_gateway_approval = self.session_core.approval_handle.is_some();
        tracing::info!(
            use_gateway_approval,
            has_approval_gate = approval_gate.is_some(),
            threshold = ?shell_threshold,
            "Shell approval gate status"
        );

        // Spawn each tool as an independent task
        let handles: Vec<tokio::task::JoinHandle<()>> = all_indices
            .iter()
            .map(|&idx| {
                let tools = self.core.all_tools.clone();
                let tc = tool_calls[idx].clone();
                let tx = tx.clone();
                let approval_handle = approval_handle.clone();
                let approval_gate = approval_gate.clone();
                let work_dir = self.session_core.current_work_dir.read().unwrap().clone();
                tokio::spawn(async move {
                    // Shell risk check: if this is a shell command and risk >= threshold,
                    // request user approval before execution.
                    let is_shell_tool = matches!(
                        tc.function.name.as_str(),
                        "bash" | "powershell" | "pwsh" | "shell"
                    );
                    if is_shell_tool {
                        // Gateway mode: use ApprovalHandle → main loop handles pause/resume
                        if use_gateway_approval {
                            if let Some(rejection) = check_shell_approval_handle(
                                &approval_handle,
                                &tc.function.name,
                                &tc.function.arguments,
                                &shell_threshold,
                                &tc.id,
                            )
                            .await
                            {
                                let _ = tx.send((idx, (rejection, false))).await;
                                return;
                            }
                        } else if let Some(ref gate) = approval_gate {
                            // CLI / test mode: use ApprovalGate trait directly
                            if let Some(rejection) = check_shell_approval(
                                gate.as_ref(),
                                &tc.function.name,
                                &tc.function.arguments,
                                &shell_threshold,
                                &tc.id,
                            )
                            .await
                            {
                                let _ = tx.send((idx, (rejection, false))).await;
                                return;
                            }
                        }
                    }

                    let result = match tokio::time::timeout(
                        tool_timeout,
                        execute_single_tool(&tools, &tc, work_dir.as_deref()),
                    )
                    .await
                    {
                        Ok((content, transient)) => (content, transient),
                        Err(_) => (
                            format!(
                                "Error: Tool '{}' timed out after {}ms",
                                tc.function.name,
                                tool_timeout.as_millis()
                            ),
                            false,
                        ),
                    };
                    let _ = tx.send((idx, result)).await;
                })
            })
            .collect();

        // Drop the remaining sender so rx.recv() returns None when all tasks complete
        drop(tx);

        // Collect results with iteration-level deadline.
        // In Gateway mode, also listen for approval requests from spawned tasks.
        let mut deadline = Instant::now() + iteration_timeout;
        let mut collected: Vec<(usize, (String, bool))> = Vec::with_capacity(all_indices.len());
        let total = all_indices.len();
        let mut interrupt: Option<ControlDecision> = None;

        if use_gateway_approval {
            // ── Gateway mode: 4-way select (results / timeout / approval / interrupt) ──
            while collected.len() < total {
                // ADR-044 §4.5: bind the *current request's* handle to a
                // local so the Arc inside it outlives the `cancelled()`
                // future. Re-read on every iteration so we observe any
                // slot swap that happened mid-tool-execution (none is
                // expected within one request — `begin_new_request` is
                // only called from `run_inner` entry — but defensive).
                let cancel_handle = self.session_core.cancel_handle();
                tokio::select! {
                    entry = rx.recv() => {
                        match entry {
                            Some((idx, result)) => collected.push((idx, result)),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(
                            "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                            iteration_timeout.as_millis(),
                            total - collected.len()
                        );
                        for handle in &handles {
                            handle.abort();
                        }
                        break;
                    }
                    approval_req = self.approval_rx.recv() => {
                        match approval_req {
                            Some((req, decision_tx)) => {
                                // Pause iteration timeout during approval wait.
                                // handle_approval_request() blocks on user decision,
                                // which can take minutes — that time should not count
                                // against the iteration deadline.
                                let remaining = deadline.saturating_duration_since(Instant::now());
                                self.handle_approval_request(req, decision_tx).await;
                                // After approval handling, check for control signals
                                // that arrived during the wait (Stop/Pause).
                                // poll_control() checks pending_interrupt set by
                                // await_approval_decision when it consumed a Stop/Pause.
                                match self.poll_control() {
                                    c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                        for handle in &handles {
                                            handle.abort();
                                        }
                                        interrupt = Some(c);
                                        break;
                                    }
                                    ControlDecision::Continue => {}
                                }
                                deadline = Instant::now() + remaining;
                            }
                            None => {
                                tracing::warn!("Approval channel closed unexpectedly");
                            }
                        }
                    }
                    // Cancellation handle — fires when external sources
                    // (MQTT `StopGeneration` dispatcher in
                    // `startup/gateway_loop.rs`, debug server Stop, CLI
                    // cancel, test harness) call `cancel()` on this
                    // session's `CancelHandle`.
                    //
                    // ADR-044 Phase 3: replaces the legacy `urgent_stop`
                    // Notify, which was only triggered in the debug path
                    // and consequently never fired under the production
                    // MQTT stop flow (the root cause of the TTFT stop bug).
                    // The handle's level-triggered `Notify` + persistent
                    // `AtomicU8` state means this branch wakes within
                    // microseconds of `cancel()` — long before the 500ms
                    // idle-poll fallback would observe the inbound Stop
                    // message.
                    //
                    // ADR-044 §4.5: this is the *current request's* handle
                    // — see `SessionCore::begin_new_request` for how the
                    // slot is swapped at every `run_inner` entry.
                    _ = cancel_handle.cancelled() => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal via cancellation handle — aborting tools");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                    // Periodic control polling during tool execution.
                    // Without this branch, a slow tool (e.g. file_read on large file)
                    // would block the select! at rx.recv() until timeout, making
                    // control signals unresponsive for potentially minutes.
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal detected during tool execution — aborting");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                }
            }
        } else {
            // ── CLI / test mode: 4-way select (results / timeout / stop / notify) ──
            while collected.len() < total {
                // ADR-044 §4.5: bind the *current request's* handle to a
                // local (see the gateway-mode branch above for the full
                // rationale on the binding pattern).
                let cancel_handle = self.session_core.cancel_handle();
                tokio::select! {
                    entry = rx.recv() => {
                        match entry {
                            Some(entry) => collected.push(entry),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(
                            "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                            iteration_timeout.as_millis(),
                            total - collected.len()
                        );
                        for handle in &handles {
                            handle.abort();
                        }
                        break;
                    }
                    _ = cancel_handle.cancelled() => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal via cancellation handle — aborting tools");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal detected during tool execution — aborting");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                }
            }
        }

        // Build final results in original order
        let results: Vec<(String, bool)> = (0..tool_calls.len())
            .map(|idx| {
                if let Some(pos) = collected.iter().find(|(i, _)| *i == idx) {
                    pos.1.clone()
                } else {
                    (
                        format!(
                            "Error: iteration timed out, tool {} not completed",
                            tool_calls[idx].function.name
                        ),
                        false,
                    )
                }
            })
            .collect();

        // If iteration timed out with incomplete tools, add a system note
        let incomplete_count = results
            .iter()
            .filter(|(r, _)| r.contains("iteration timed out"))
            .count();
        if incomplete_count > 0 {
            tracing::warn!(
                incomplete_count,
                "Iteration timed out with incomplete tool(s)"
            );
        }

        (results, interrupt)
    }
}

/// Execute a single tool call against the tool registry.
///
/// Returns `(content, is_transient)` where `is_transient` indicates whether
/// the tool result should be treated as one-shot (ADR-032 C3a).
/// Tools that retrieve pre-existing data (e.g. context_recall) return
/// transient results that are injected once without permanently appending
/// to history.  Error paths that don't reach the tool's `execute()` method
/// always return `is_transient = false`.
pub(crate) async fn execute_single_tool(
    tools: &[Arc<dyn Tool>],
    tool_call: &ToolCall,
    work_dir: Option<&str>,
) -> (String, bool) {
    let tool_name = &tool_call.function.name;
    let params_str = &tool_call.function.arguments;

    // Parse arguments before tool lookup — we need to detect the
    // TOOL_CALL_INCOMPLETE marker (valid JSON injected by streaming assembler)
    // and reject genuinely unparseable arguments (e.g. LLM hallucinated output).
    // Early return on parse failure avoids the old silent "{}" degradation
    // that caused LLM retry loops.
    let params: serde_json::Value = match serde_json::from_str(params_str) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                tool = %tool_name,
                params_len = params_str.len(),
                params_preview = %&params_str[..params_str.len().min(200)],
                error = %e,
                "Failed to parse tool call arguments as JSON — returning error to LLM"
            );
            return (
                format!(
                    "Error: Tool '{}' arguments are not valid JSON and could not be parsed: {}. \
                     This call was NOT executed. \
                     Arguments preview (first 200 bytes): {}",
                    tool_name,
                    e,
                    &params_str[..params_str.len().min(200)]
                ),
                false,
            );
        }
    };

    // Detect truncated/incomplete tool calls: the streaming assembler injects a
    // special error marker when arguments are truncated. Skip actual execution
    // and return a clear error so the LLM knows to regenerate (not retry blindly).
    if let Some("TOOL_CALL_INCOMPLETE") = params.get("error").and_then(|v| v.as_str()) {
        return (
            params
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Tool call arguments were truncated during streaming")
                .to_string(),
            false,
        );
    }

    // Find the tool
    let tool = tools.iter().find(|t| {
        let spec = t.spec();
        spec.name == *tool_name
    });

    match tool {
            Some(tool) => match tool.execute(params, work_dir).await {
                Ok(result) => {
        // ADR-032: `context_recall` returns transient tool results — the
        // full recalled content is injected into the next LLM request only
        // (via `pending_transient_tool_msgs`) and is NEVER appended to
        // permanent history. This breaks the
        // recall → compress → recall loop that would otherwise form when
        // the recalled content exceeds `soft_threshold_chars` again on the
        // following iteration.
        let transient = tool_name == "context_recall";
                    let content = if result.ok {
                    result.content
                } else {
                    // Include both the error output (stdout/stderr) and the error code
                    if result.content.is_empty() {
                        format!("Error: {}", result.error.unwrap_or_default())
                    } else {
                        format!(
                            "{}\nError: {}",
                            result.content,
                            result.error.as_deref().unwrap_or("unknown error")
                        )
                    }
                };
                (content, transient)
            }
            Err(e) => (format!("Tool execution error: {e}"), false),
        },
        None => (format!("Unknown tool: {tool_name}"), false),
    }
}

/// Check if a shell command requires user approval based on risk assessment.
///
/// Returns `Some(error_message)` if the command was rejected by the user,
/// or `None` if the command can proceed (approved or below threshold).
async fn check_shell_approval(
    gate: &dyn ApprovalGate,
    tool_name: &str,
    params_json: &str,
    threshold: &ShellApprovalThreshold,
    tool_call_id: &str,
) -> Option<String> {
    // "Never" threshold: skip approval entirely
    if *threshold == ShellApprovalThreshold::Never {
        return None;
    }

    // Parse the command from shell tool params
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("check_shell_approval: failed to parse shell params: {}", e);
            return None; // Can't parse params, let the tool handle the error
        }
    };

    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    // Assess risk (with no provenance lookup in the spawned task context)
    let assessment = shell_risk::assess_shell_risk(command, |_path| None);

    // Convert ShellApprovalThreshold to ShellRisk for comparison
    let threshold_risk = match threshold {
        ShellApprovalThreshold::Low => ShellRisk::Low,
        ShellApprovalThreshold::Medium => ShellRisk::Medium,
        ShellApprovalThreshold::High => ShellRisk::High,
        ShellApprovalThreshold::Never => unreachable!(), // handled above
    };

    // Check if risk meets/exceeds threshold
    if !risk_meets_threshold(assessment.risk, threshold_risk) {
        return None; // Risk below threshold, proceed normally
    }

    // Blocked commands: always reject without asking
    if assessment.risk == ShellRisk::Blocked {
        return Some(format!(
            "Error: Shell command was blocked for safety reasons. Command: {}\nReason: {}",
            command, assessment.reason
        ));
    }

    // Build approval request
    let approval_req = ApprovalRequest {
        tool_name: tool_name.to_string(),
        action: command.to_string(),
        risk_level: assessment.risk,
        reason: assessment.reason.clone(),
        executable_paths: assessment.executable_paths.clone(),
        provenance_elevated: assessment.provenance_elevated,
        tool_call_id: tool_call_id.to_string(),
    };

    tracing::info!(
        risk = %assessment.risk.label(),
        command = %command,
        reason = %assessment.reason,
        "Requesting user approval for shell command"
    );

    // Request approval
    let response = gate.request_approval(&approval_req).await;

    match response {
        crate::security::approval_gate::ApprovalResponse::Approved => {
            tracing::info!(command = %command, "Shell command approved by user");
            None
        }
        crate::security::approval_gate::ApprovalResponse::Rejected => {
            tracing::info!(command = %command, "Shell command rejected by user");
            Some(format!(
                "Error: Shell command was rejected by the user based on risk assessment.\n\
                 Command: {}\nRisk level: {}\nReason: {}",
                command,
                assessment.risk.label(),
                assessment.reason
            ))
        }
        crate::security::approval_gate::ApprovalResponse::AlwaysAllow { .. } => {
            tracing::info!(command = %command, "Shell command approved (always allow)");
            None
        }
    }
}

/// Check if a given risk level meets or exceeds the threshold.
/// Risk ordering: Low < Medium < High < Blocked.
fn risk_meets_threshold(risk: ShellRisk, threshold: ShellRisk) -> bool {
    fn risk_ordinal(r: ShellRisk) -> u8 {
        match r {
            ShellRisk::Low => 0,
            ShellRisk::Medium => 1,
            ShellRisk::High => 2,
            ShellRisk::Blocked => 3,
        }
    }
    risk_ordinal(risk) >= risk_ordinal(threshold)
}

/// Check if a shell command requires user approval via ApprovalHandle (Gateway mode).
///
/// Same risk assessment as `check_shell_approval`, but uses the unified pause
/// architecture: sends request to AgentLoop main loop via ApprovalHandle mpsc,
/// which emits ChunkEvent::ToolApprovalNeeded to Gateway and blocks on inbound_rx
/// until the user's ApprovalDecision arrives (no timeout).
async fn check_shell_approval_handle(
    handle: &ApprovalHandle,
    tool_name: &str,
    params_json: &str,
    threshold: &ShellApprovalThreshold,
    tool_call_id: &str,
) -> Option<String> {
    // "Never" threshold: skip approval entirely
    if *threshold == ShellApprovalThreshold::Never {
        return None;
    }

    // Parse the command from shell tool params
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "check_shell_approval_handle: failed to parse shell params: {}",
                e
            );
            return None;
        }
    };

    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    // Assess risk (with no provenance lookup in the spawned task context)
    let assessment = shell_risk::assess_shell_risk(command, |_path| None);

    // Convert ShellApprovalThreshold to ShellRisk for comparison
    let threshold_risk = match threshold {
        ShellApprovalThreshold::Low => ShellRisk::Low,
        ShellApprovalThreshold::Medium => ShellRisk::Medium,
        ShellApprovalThreshold::High => ShellRisk::High,
        ShellApprovalThreshold::Never => unreachable!(),
    };

    // Check if risk meets/exceeds threshold
    if !risk_meets_threshold(assessment.risk, threshold_risk) {
        return None;
    }

    // Blocked commands: always reject without asking
    if assessment.risk == ShellRisk::Blocked {
        return Some(format!(
            "Error: Shell command was blocked for safety reasons. Command: {}\nReason: {}",
            command, assessment.reason
        ));
    }

    // Build approval request
    let approval_req = ApprovalRequest {
        tool_name: tool_name.to_string(),
        action: command.to_string(),
        risk_level: assessment.risk,
        reason: assessment.reason.clone(),
        executable_paths: assessment.executable_paths.clone(),
        provenance_elevated: assessment.provenance_elevated,
        tool_call_id: tool_call_id.to_string(),
    };

    tracing::info!(
        risk = %assessment.risk.label(),
        command = %command,
        reason = %assessment.reason,
        "Requesting user approval for shell command (Gateway mode)"
    );

    // Request approval via ApprovalHandle (no timeout — main loop blocks on inbound_rx)
    let decision = handle.request_approval(approval_req).await;

    if decision.approved {
        tracing::info!(command = %command, "Shell command approved by user");
        None
    } else {
        let reject_reason = decision.reason.as_deref().unwrap_or("rejected by user");
        tracing::info!(command = %command, reason = %reject_reason, "Shell command rejected");
        Some(format!(
            "Error: Shell command was rejected based on risk assessment.\n\
             Command: {}\nRisk level: {}\nReason: {}",
            command,
            assessment.risk.label(),
            reject_reason
        ))
    }
}

impl AgentLoop {
    // ── Tool pipeline methods (ADR-014 Phase 8: extracted from execute_single_iteration) ──

    /// Prepare tool calls for execution.
    ///
    /// Persists think block to JSONL (via D2 dedup helper), deduplicates
    /// same-iteration tool calls, appends assistant message to history,
    /// persists tool calls to JSONL, and emits ToolCall chunk events.
    ///
    /// Returns the deduplicated tool call list.
    pub(crate) fn prepare_tool_calls(
        &mut self,
        response: &acowork_core::providers::traits::ChatResponse,
    ) -> Vec<ToolCall> {
        // ADR-022: Flush the current streaming line to JSONL.
        //
        // During streaming, the provider layer split think tags into
        // ReasoningContent + Content events, and each role transition
        // already flushed the previous line via flush_and_new_streaming_line.
        // This final flush captures any remaining content in the last
        // streaming line (e.g., the assistant text that preceded the
        // tool_call).
        let flushed = self.session_core.flush_streaming_line(self.session.conversation.as_ref());
        tracing::info!(
            flushed_role = flushed.as_ref().map(|c| {
                // We can't easily get the role here, but we can log content length
                c.len()
            }).unwrap_or(0),
            flushed_len = flushed.as_ref().map(|c| c.len()).unwrap_or(0),
            "ADR-022 prepare_tool_calls: flush_streaming_line result"
        );

        // ADR-022: Only use legacy persistence if no streaming flush occurred.
        // When streaming already flushed thought content on role transitions,
        // persist_think_to_conversation would create a duplicate entry.
        let streamed = self.session_core.streaming_flush_count.load(Ordering::Relaxed) > 0;
        tracing::info!(
            streamed,
            streaming_flush_count = self.session_core.streaming_flush_count.load(Ordering::Relaxed),
            has_reasoning = response.reasoning_content.is_some(),
            reasoning_len = response.reasoning_content.as_ref().map(|r| r.len()).unwrap_or(0),
            "ADR-022 prepare_tool_calls: streamed={}",
            streamed
        );

        if !streamed {
            // Legacy path: persist think block from the final response.
            // Handles providers that use reasoning_content field (DeepSeek)
            // or  thinking... response tags without streaming them.
            if let Some(ref conversation) = self.session.conversation {
                crate::agent::loop_session::persist_think_to_conversation(conversation, response);
            }
        }

        // Has tool calls — process them
        let tool_calls = response.tool_calls.clone().unwrap_or_default();

        // Tool call deduplication (same iteration)
        let mut seen = HashSet::new();
        let deduped_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| {
                let sig = format!("{}:{}", tc.function.name, tc.function.arguments);
                seen.insert(sig)
            })
            .collect();

        // Persist assistant turn to in-memory history.
        //
        // IMPORTANT: We intentionally store `reasoning_content` here, despite it
        // looking like internal reasoning detail, **because the DeepSeek thinking
        // mode API requires it on the wire**: when an assistant turn contains
        // tool_calls, the `reasoning_content` for that turn MUST be echoed back
        // to the DeepSeek API in all subsequent user turns — otherwise the API
        // returns HTTP 400. Reference: https://api-docs.deepseek.com/zh-cn/guides/thinking_mode
        // section "工具调用 → 必须完整回传 reasoning_content".
        //
        // For the OpenAI-compatible code path, `reasoning_content` is naturally
        // None on non-DeepSeek endpoints (o1/o3 Chat Completions do not surface
        // reasoning in the response), so serializing the field back is a no-op
        // for non-DeepSeek providers.
        //
        // Why this is NOT a bug: looking at it in isolation, storing
        // reasoning_content into history seems risky (it grows context); it is
        // NOT optional for DeepSeek, and removing it will silently break
        // DeepSeek tool-call iterations with 400 errors. See the companion
        // comment in loop_session.rs::handle_text_response for the matching
        // text-only-path behavior.
        self.session.history.append(ChatMessage {
            reasoning_content: response.reasoning_content.clone(),
            tool_calls: Some(deduped_calls.clone()),
            ..ChatMessage::assistant(response.content.clone())
        });

        // Persist tool calls to JSONL
        if let Some(ref conversation) = self.session.conversation {
            for tc in &deduped_calls {
                let metadata = serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_call_id": tc.id,
                });
                // ADR-035 M2: pre-assign a stable message_id so the JSONL
                // entry and the record_complete event share the same id.
                let message_id = uuid::Uuid::new_v4().to_string();
                conversation.append_message_with_id(
                    "tool_call",
                    &tc.function.arguments,
                    Some(metadata),
                    Some(message_id.clone()),
                );
                // ADR-035 C1: emit record_complete so the frontend inserts
                // the tool_call directly into messages[] (no streaming phase).
                // Tool metadata is forwarded here so the frontend can pair
                // the eventual tool_result without an extra HTTP fetch.
                //
                // `seq` is the per-session monotonic counter from
                // `SessionCore::next_seq()`. Each tool_call gets its own seq
                // so the four parallel reads are ordered correctly even under
                // cross-publisher reorder; the Desktop inserts them at the
                // correct position in `messages[]` via `insertBySeq`.
                let _ = self.session_core.try_send_chunk(
                    crate::agent::loop_::ChunkEvent::RecordComplete {
                        session_id: self.session_core.session_id.clone().unwrap_or_default(),
                        role: "tool_call".to_string(),
                        message_id,
                        content: tc.function.arguments.clone(),
                        tool_name: tc.function.name.clone(),
                        tool_call_id: tc.id.clone(),
                        is_error: false,
                        seq: self.session_core.next_seq(),
                    },
                );
            }
        }

        deduped_calls
    }

    /// Pre-execution loop detection: check tool calls before executing them.
    ///
    /// Tool calls that trigger Block or Break level are filtered out;
    /// Warning-level calls are allowed through (handled post-execution).
    ///
    /// Returns `(calls_to_execute, blocked_info)` where blocked_info
    /// contains the original index and detected pattern for each blocked call.
    pub(crate) fn pre_check_loop_detection(
        &mut self,
        deduped_calls: &[ToolCall],
    ) -> (Vec<ToolCall>, Vec<(usize, LoopPattern)>) {
        let mut calls_to_execute: Vec<ToolCall> = Vec::new();
        let mut blocked_info: Vec<(usize, LoopPattern)> = Vec::new();

        for (idx, tc) in deduped_calls.iter().enumerate() {
            match self
                .session
                .loop_detector
                .peek_check(&tc.function.name, &tc.function.arguments)
            {
                LoopDetectionResult::NoLoop => {
                    calls_to_execute.push(tc.clone());
                }
                LoopDetectionResult::LoopDetected { level, pattern, .. } => {
                    match level {
                        ResponseLevel::Warning => {
                            // Warning is handled post-execution; allow the call
                            calls_to_execute.push(tc.clone());
                        }
                        ResponseLevel::Block | ResponseLevel::Break => {
                            tracing::warn!(
                                tool = %tc.function.name,
                                level = ?level,
                                "Loop detected (pre-execution), blocking tool call"
                            );
                            blocked_info.push((idx, pattern));
                        }
                    }
                }
            }
        }

        (calls_to_execute, blocked_info)
    }

    /// Dispatch tool calls for execution and merge results.
    ///
    /// Intercepts special tools (ask_user_question, todo_write) for sequential
    /// processing, executes remaining tools in parallel, then merges all
    /// results back in original order. Also merges with pre-blocked results
    /// from `pre_check_loop_detection`.
    ///
    /// Returns `(tool_results, interrupt)` where each tool_result is `(content, is_transient)`.
    /// `is_transient` propagates from `ToolResult.transient` (ADR-032 C3a).
    pub(crate) async fn dispatch_and_merge_tools(
        &mut self,
        calls_to_execute: Vec<ToolCall>,
        deduped_calls: &[ToolCall],
        blocked_info: &[(usize, LoopPattern)],
        context_builder: &mut ContextBuilder,
    ) -> (Vec<(String, bool)>, Option<ControlDecision>) {
        // Intercept special tools
        let mut ask_question_results: Vec<(usize, (String, bool))> = Vec::new();
        let mut todo_write_results: Vec<(usize, (String, bool))> = Vec::new();
        let mut parallel_calls: Vec<(usize, ToolCall)> = Vec::new();

        for (idx, tc) in calls_to_execute.into_iter().enumerate() {
            if tc.function.name == "ask_user_question" {
                let result = self.handle_ask_user_question(&tc).await;
                ask_question_results.push((idx, (result, false)));
            } else if tc.function.name == "todo_write" {
                let result = self.handle_todo_write(&tc, context_builder);
                todo_write_results.push((idx, (result, false)));
            } else {
                parallel_calls.push((idx, tc));
            }
        }

        // Execute non-question tools in parallel
        let calls_for_parallel: Vec<ToolCall> =
            parallel_calls.iter().map(|(_, tc)| tc.clone()).collect();
        let (parallel_results, interrupt) =
            self.execute_tools_parallel(&calls_for_parallel).await;

        // Merge results: ask_question + todo_write + parallel, mapped back to original indices
        let ask_result_map: std::collections::HashMap<usize, (String, bool)> =
            ask_question_results.into_iter().collect();
        let todo_result_map: std::collections::HashMap<usize, (String, bool)> =
            todo_write_results.into_iter().collect();

        let mut final_results: Vec<(usize, (String, bool))> = Vec::new();
        for (parallel_idx, result) in parallel_results.into_iter().enumerate() {
            if let Some((orig_idx, _)) = parallel_calls.get(parallel_idx) {
                final_results.push((*orig_idx, result));
            }
        }
        for (orig_idx, result) in &ask_result_map {
            final_results.push((*orig_idx, result.clone()));
        }
        for (orig_idx, result) in &todo_result_map {
            final_results.push((*orig_idx, result.clone()));
        }
        final_results.sort_by_key(|(idx, _)| *idx);

        let executed_results: Vec<(String, bool)> =
            final_results.into_iter().map(|(_, r)| r).collect();

        // Merge executed results with pre-blocked results, preserving original order
        let mut tool_results: Vec<(String, bool)> = Vec::with_capacity(deduped_calls.len());
        let mut executed_iter = executed_results.into_iter();
        for idx in 0..deduped_calls.len() {
            if let Some(pos) = blocked_info.iter().position(|(i, _)| *i == idx) {
                let msg = match &blocked_info[pos].1 {
                    LoopPattern::SameToolFlood => {
                        "Loop detected: this tool has been called too many times in a short period. \
                         Please STOP using this tool and try a different approach \
                         (e.g., use file_read to verify results, or switch to another tool)."
                    }
                    _ => {
                        "Loop detected: this tool call has been blocked because it was repeated too many times with the same parameters. Try a different approach."
                    }
                };
                // Blocked results are never transient
                tool_results.push((msg.to_string(), false));
            } else {
                tool_results.push(executed_iter.next().unwrap_or(("".to_string(), false)));
            }
        }

        (tool_results, interrupt)
    }

    /// Persist tool results to JSONL and emit ToolResult chunk events.
    ///
    /// ADR-035 D9.2: tool_result content is truncated to first 5 lines at
    /// the MQTT publish layer (full content stays in JSONL).
    ///
    /// `is_error` is derived heuristically from the result content. The
    /// runtime's error paths produce results starting with `"Error"` or
    /// `"Loop detected"` (see `execute_single_tool` and the blocked-info
    /// branch in `dispatch_and_merge_tools`). The frontend uses this flag
    /// to color the tool_result bubble red and to compute the success/
    /// failure counter on the tool_call pairing header.
    pub(crate) fn persist_and_emit_tool_results(
        &mut self,
        deduped_calls: &[ToolCall],
        tool_results: &[String],
    ) {
        // Persist tool results to JSONL
        if let Some(ref conversation) = self.session.conversation {
            for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                let metadata = serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_call_id": tc.id,
                });
                // ADR-035 M2: pre-assign a stable message_id so the JSONL
                // entry (full content) and the record_complete event
                // (truncated to 5 lines at publish time, D9.2) share the
                // same id.
                let message_id = uuid::Uuid::new_v4().to_string();
                conversation.append_message_with_id(
                    "tool_result",
                    result_content,
                    Some(metadata),
                    Some(message_id.clone()),
                );
                // ADR-035 C1/D9.2: emit record_complete with the FULL
                // content; the MQTT publish layer truncates tool_result to
                // the first 5 lines for frontend display. JSONL keeps the
                // full content for LLM context.
                let is_error = looks_like_tool_error(result_content);
                // Each tool_result gets a fresh seq that fits after its
                // corresponding tool_call's seq. Both source-of-truth
                // ordering (JSONL append order) and the chunk_relay FIFO
                // guarantee the relative order here; the seq just makes the
                // Desktop's reorder-defensive `insertBySeq` self-healing.
                let _ = self.session_core.try_send_chunk(
                    crate::agent::loop_::ChunkEvent::RecordComplete {
                        session_id: self.session_core.session_id.clone().unwrap_or_default(),
                        role: "tool_result".to_string(),
                        message_id,
                        content: result_content.clone(),
                        tool_name: tc.function.name.clone(),
                        tool_call_id: tc.id.clone(),
                        is_error,
                        seq: self.session_core.next_seq(),
                    },
                );
            }
        }
    }

    /// Post-execution loop detection.
    ///
    /// Checks tool results for loop patterns, appends deferred warning
    /// messages to history after all tool results, and returns
    /// `Err(RuntimeError::LoopDetected)` if a Break-level loop is detected.
    /// Skips already-blocked tool calls to avoid false positives.
    pub(crate) fn post_check_loop_detection(
        &mut self,
        deduped_calls: &[ToolCall],
        tool_results: &[String],
        blocked_info: &[(usize, LoopPattern)],
    ) -> Result<()> {
        let mut deferred_warnings: Vec<String> = Vec::new();
        let mut break_error: Option<String> = None;

        for (idx, (tc, result_content)) in deduped_calls.iter().zip(tool_results.iter()).enumerate()
        {
            // Skip loop detection for pre-blocked tool calls to avoid self-reinforcing
            // false positives: blocked tools return uniform error messages whose identical
            // hashes would incorrectly trigger NoProgress detection.
            if blocked_info.iter().any(|(i, _)| *i == idx) {
                continue;
            }

            match self.session.loop_detector.check(
                &tc.function.name,
                &tc.function.arguments,
                result_content,
            ) {
                LoopDetectionResult::NoLoop => {}
                LoopDetectionResult::LoopDetected {
                    pattern,
                    level,
                    count: _,
                    message,
                } => {
                    tracing::warn!(message = %message, level = ?level, "Loop detected");
                    match level {
                        ResponseLevel::Warning => {
                            let warning_content = match &pattern {
                                LoopPattern::SameToolFlood => {
                                    format!(
                                        "[System Warning] {message} \
                                         This tool has been called excessively. \
                                         Please STOP using this tool and try a different approach \
                                         (e.g., use file_read to verify results, or switch to another tool) \
                                         to complete the task."
                                    )
                                }
                                _ => format!("[System Warning] {message}"),
                            };
                            deferred_warnings.push(warning_content);
                        }
                        ResponseLevel::Block => {
                            // Block was already handled by returning error as tool result
                        }
                        ResponseLevel::Break => {
                            break_error = Some(message);
                            break;
                        }
                    }
                }
            }
        }

        // Append deferred warning messages AFTER all tool results
        for warning_content in deferred_warnings {
            self.session.history.append(ChatMessage {
                role: acowork_core::providers::traits::MessageRole::User,
                content: warning_content,
                name: Some("system".to_string()),
                ..Default::default()
            });
        }

        // Handle Break-level loop detection
        if let Some(msg) = break_error {
            self.transition_status(SessionStatus::Idle);
            return Err(RuntimeError::LoopDetected(msg));
        }

        Ok(())
    }
}

/// Heuristic detection of a tool error from its result content.
///
/// The runtime's error paths produce results that begin with one of the
/// well-known prefixes below. We use this so the MQTT `record_complete`
/// payload can carry `is_error = true` without restructuring
/// `execute_tools_parallel`'s return type. The frontend treats the flag
/// as best-effort: any result that renders the tool_call as failed will
/// be styled accordingly, and a `false` here just means we have no
/// positive signal (tool may still have failed in ways we did not tag).
fn looks_like_tool_error(result_content: &str) -> bool {
    let trimmed = result_content.trim_start();
    trimmed.starts_with("Error")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("Loop detected")
        || trimmed.starts_with("Tool call")
}
