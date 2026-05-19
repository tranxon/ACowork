//! Tool execution module
//!
//! Extracted from [`super::loop_`] to provide reusable parallel tool execution
//! shared between production [`AgentLoop::run`] and debug `DebugSessionTask`.
//!
//! Handles:
//! - Tool activation filtering (done at registry build time — no runtime check needed)
//! - Parallel execution with per-tool timeout + iteration deadline
//! - Result collection with original ordering

use std::sync::Arc;

use rollball_core::providers::traits::ToolCall;
use rollball_core::tools::traits::Tool;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::security::approval_gate::{ApprovalGate, ApprovalRequest};
use crate::security::shell_risk::{self, ShellRisk};

use super::loop_::AgentLoop;

impl AgentLoop {
    /// Execute tool calls in parallel with per-tool timeout and iteration-level deadline.
    ///
    /// Tool activation filtering is done at registry build time via
    /// [`ToolRegistry::activate`] — no runtime permission check needed here.
    ///
    /// Returns results in the same order as input tool calls.
    /// Individual tool failures are captured as error strings, not propagated.
    pub(crate) async fn execute_tools_parallel(
        &self,
        tool_calls: &[ToolCall],
    ) -> Vec<String> {
        if tool_calls.is_empty() {
            return Vec::new();
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

        let tool_timeout = Duration::from_millis(self.core.config.tool_timeout_ms);
        let iteration_timeout =
            Duration::from_millis(self.core.config.iteration_timeout_ms);

        // Channel to collect results from spawned tasks
        let (tx, mut rx) = mpsc::channel::<(usize, String)>(tool_calls.len());

        // Clone shared state for spawned tasks
        let approval_gate = self.core.approval_gate.clone();
        let shell_threshold = self.core.shell_approval_threshold.clone();

        // Spawn each tool as an independent task
        let handles: Vec<tokio::task::JoinHandle<()>> = all_indices
            .iter()
            .map(|&idx| {
                let tools = self.core.tools.clone();
                let tc = tool_calls[idx].clone();
                let tx = tx.clone();
                let approval_gate = approval_gate.clone();
                let shell_threshold = shell_threshold.clone();
                tokio::spawn(async move {
                    // Shell risk check: if this is a shell command and risk >= threshold,
                    // request user approval via the ApprovalGate before execution.
                    if tc.function.name == "shell" {
                        if let Some(ref gate) = approval_gate {
                            if let Some(rejection) = check_shell_approval(
                                gate.as_ref(),
                                &tc.function.arguments,
                                &shell_threshold,
                            )
                            .await
                            {
                                let _ = tx.send((idx, rejection)).await;
                                return;
                            }
                        }
                    }

                    let result = match tokio::time::timeout(
                        tool_timeout,
                        execute_single_tool(&tools, &tc),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => format!(
                            "Error: Tool '{}' timed out after {}ms",
                            tc.function.name,
                            tool_timeout.as_millis()
                        ),
                    };
                    let _ = tx.send((idx, result)).await;
                })
            })
            .collect();

        // Drop the remaining sender so rx.recv() returns None when all tasks complete
        drop(tx);

        // Collect results with iteration-level deadline
        let deadline = Instant::now() + iteration_timeout;
        let mut collected: Vec<(usize, String)> =
            Vec::with_capacity(all_indices.len());
        let total = all_indices.len();

        while collected.len() < total {
            tokio::select! {
                // A result arrived from a spawned task
                entry = rx.recv() => {
                    match entry {
                        Some((idx, result)) => collected.push((idx, result)),
                        None => break, // All senders dropped
                    }
                }
                // Iteration-level deadline exceeded
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                        iteration_timeout.as_millis(),
                        total - collected.len()
                    );
                    // Abort all remaining spawned tasks
                    for handle in &handles {
                        handle.abort();
                    }
                    break;
                }
            }
        }

        // Build final results in original order
        let results: Vec<String> = (0..tool_calls.len())
            .map(|idx| {
                if let Some(pos) =
                    collected.iter().find(|(i, _)| *i == idx)
                {
                    pos.1.clone()
                } else {
                    format!(
                        "Error: iteration timed out, tool {} not completed",
                        tool_calls[idx].function.name
                    )
                }
            })
            .collect();

        // If iteration timed out with incomplete tools, add a system note
        let incomplete_count = results
            .iter()
            .filter(|r| r.contains("iteration timed out"))
            .count();
        if incomplete_count > 0 {
            tracing::warn!(
                incomplete_count,
                "Iteration timed out with incomplete tool(s)"
            );
        }

        results
    }
}

/// Execute a single tool call against the tool registry.
///
/// Returns the result content string (success or error message).
pub(crate) async fn execute_single_tool(tools: &[Arc<dyn Tool>], tool_call: &ToolCall) -> String {
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
            return format!(
                "Error: Tool '{}' arguments are not valid JSON and could not be parsed: {}. \
                 This call was NOT executed. \
                 Arguments preview (first 200 bytes): {}",
                tool_name,
                e,
                &params_str[..params_str.len().min(200)]
            );
        }
    };

    // Detect truncated/incomplete tool calls: the streaming assembler injects a
    // special error marker when arguments are truncated. Skip actual execution
    // and return a clear error so the LLM knows to regenerate (not retry blindly).
    if let Some("TOOL_CALL_INCOMPLETE") =
        params.get("error").and_then(|v| v.as_str())
    {
        return params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Tool call arguments were truncated during streaming")
            .to_string();
    }

    // Find the tool
    let tool = tools.iter().find(|t| {
        let spec = t.spec();
        spec.name == *tool_name
    });

    match tool {
        Some(tool) => match tool.execute(params).await {
            Ok(result) => {
                if result.ok {
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
                }
            }
            Err(e) => format!("Tool execution error: {e}"),
        },
        None => format!("Unknown tool: {tool_name}"),
    }
}

/// Check if a shell command requires user approval based on risk assessment.
///
/// Returns `Some(error_message)` if the command was rejected by the user,
/// or `None` if the command can proceed (approved or below threshold).
async fn check_shell_approval(
    gate: &dyn ApprovalGate,
    params_json: &str,
    threshold: &str,
) -> Option<String> {
    // Parse the command from shell tool params
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(p) => p,
        Err(_) => return None, // Can't parse params, let the tool handle the error
    };

    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    // Assess risk (with no provenance lookup in the spawned task context)
    let assessment = shell_risk::assess_shell_risk(command, |_path| None);

    // Parse threshold string to ShellRisk
    let threshold_risk = match threshold.to_lowercase().as_str() {
        "low" => ShellRisk::Low,
        "medium" => ShellRisk::Medium,
        "high" => ShellRisk::High,
        "never" => return None, // Never require approval
        _ => ShellRisk::Medium,  // Default to medium
    };

    // Check if risk meets/exceeds threshold
    if !risk_meets_threshold(assessment.risk, threshold_risk) {
        return None; // Risk below threshold, proceed normally
    }

    // Blocked commands: always reject without asking
    if assessment.risk == ShellRisk::Blocked {
        return Some(format!(
            "Error: Shell command execution was blocked for safety reasons. {}",
            assessment.reason
        ));
    }

    // Build approval request
    let approval_req = ApprovalRequest {
        tool_name: "shell".to_string(),
        action: command.to_string(),
        risk_level: assessment.risk,
        reason: assessment.reason.clone(),
        executable_paths: assessment.executable_paths.clone(),
        provenance_elevated: assessment.provenance_elevated,
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
                "Error: Shell command execution was rejected by the user based on risk assessment. \
                 Risk level: {}, Reason: {}",
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
