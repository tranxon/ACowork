//! Agent main loop (9 steps)
//!
//! The core execution loop for Agent Runtime.
//! References ZeroClaw agent/loop_.rs but simplified for IPC architecture.

use std::collections::HashSet;
use std::sync::Arc;

use rollball_core::providers::traits::{
    ChatMessage, MessageRole, Provider, ToolCall,
};
use rollball_core::tools::traits::Tool;

use crate::agent::budget_guard::{BudgetCheckResult, BudgetGuard};
use crate::agent::context::ContextBuilder;
use crate::agent::history::HistoryManager;
use crate::agent::loop_detector::{LoopDetectionResult, LoopDetector, ResponseLevel};
use crate::config::RuntimeConfig;
use crate::error::{Result, RuntimeError};

/// Agent loop runner
pub struct AgentLoop {
    /// Runtime configuration
    config: RuntimeConfig,
    /// Agent manifest
    manifest: rollball_core::AgentManifest,
    /// LLM Provider
    provider: Arc<dyn Provider>,
    /// Tool registry
    tools: Vec<Arc<dyn Tool>>,
    /// History manager
    history: HistoryManager,
    /// Budget guard
    budget_guard: BudgetGuard,
    /// Loop detector
    loop_detector: LoopDetector,
}

impl AgentLoop {
    /// Create a new agent loop runner
    pub fn new(
        config: RuntimeConfig,
        manifest: rollball_core::AgentManifest,
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        budget: rollball_core::Budget,
    ) -> Self {
        let max_tokens = config.history_max_tokens;
        let keep_full = config.keep_full_results;
        Self {
            config,
            manifest,
            provider,
            tools,
            history: HistoryManager::new(max_tokens, keep_full),
            budget_guard: BudgetGuard::new(budget),
            loop_detector: LoopDetector::with_defaults(),
        }
    }

    /// Run the agent loop for a single user message
    pub async fn run(&mut self, user_message: &str, context_builder: &ContextBuilder) -> Result<String> {
        // Add user message to history
        self.history.append(ChatMessage {
            role: MessageRole::User,
            content: user_message.to_string(),
            name: None,
            tool_calls: None,
        });

        let mut iteration = 0u32;

        loop {
            iteration += 1;
            tracing::info!(iteration, "Starting loop iteration");

            // ⑨ Iteration limit check
            if iteration > self.config.max_iterations {
                tracing::warn!(iteration, "Max iterations reached");
                return Ok("Maximum iterations reached. The agent stopped to prevent infinite looping.".to_string());
            }

            // ① Budget pre-check
            let estimated_tokens = self.history.estimate_total_tokens() + 500; // +500 for new response
            match self.budget_guard.check(estimated_tokens) {
                BudgetCheckResult::Allowed => {}
                BudgetCheckResult::Exceeded { reason, action } => {
                    tracing::warn!(reason = %reason, action = %action, "Budget exceeded");
                    match action.as_str() {
                        "deny" => {
                            return Err(RuntimeError::BudgetExceeded(reason));
                        }
                        "warn" => {
                            self.history.append(ChatMessage {
                                role: MessageRole::System,
                                content: format!("Warning: {reason}"),
                                name: None,
                                tool_calls: None,
                            });
                        }
                        _ => {}
                    }
                }
            }

            // ② Build context
            // Note: AgentLoop needs the manifest from the loaded package.
            // For now, we store it internally.
            let chat_request = context_builder.build(&self.manifest, &self.history);

            // ②.5 Preemptive trim
            self.history.preemptive_trim(self.config.history_max_tokens);

            // ③ Call LLM
            let response = match self.provider.chat(chat_request).await {
                Ok(resp) => resp,
                Err(e) => {
                    let err_msg = e.to_string();
                    // ③ Reactive Recovery: if context overflow, emergency trim and retry
                    if err_msg.contains("context_length_exceeded")
                        || err_msg.contains("max_tokens")
                        || err_msg.contains("token limit")
                    {
                        tracing::warn!("Context overflow detected, attempting emergency trim");
                        let removed = self.history.emergency_trim();
                        if removed > 0 {
                            tracing::info!("Emergency trim removed {} messages, retrying", removed);
                            let chat_request = context_builder.build(&self.manifest, &self.history);
                            match self.provider.chat(chat_request).await {
                                Ok(resp) => resp,
                                Err(e2) => {
                                    tracing::error!(error = %e2, "LLM call failed after emergency trim");
                                    return Err(RuntimeError::Provider(e2.to_string()));
                                }
                            }
                        } else {
                            tracing::error!(error = %e, "LLM call failed: context overflow, no trimmable history");
                            return Err(RuntimeError::Provider(err_msg));
                        }
                    } else {
                        tracing::error!(error = %e, "LLM call failed");
                        return Err(RuntimeError::Provider(err_msg));
                    }
                }
            };

            // ④ Parse response
            let has_tool_calls = response.tool_calls.is_some();

            // Update budget
            if let Some(usage) = &response.usage {
                self.budget_guard.update_usage(usage.total_tokens, 0.0);
            }

            if !has_tool_calls {
                // Pure text response — normal exit
                let content = response.content.clone();
                self.history.append(ChatMessage {
                    role: MessageRole::Assistant,
                    content: response.content,
                    name: None,
                    tool_calls: None,
                });

                tracing::info!(iteration, "Agent returned text response");
                return Ok(content);
            }

            // Has tool calls — process them
            let tool_calls = response.tool_calls.unwrap_or_default();

            // ④.5 Tool call deduplication (same iteration)
            let mut seen = HashSet::new();
            let deduped_calls: Vec<ToolCall> = tool_calls
                .into_iter()
                .filter(|tc| {
                    let sig = format!("{}:{}", tc.function.name, tc.function.arguments);
                    seen.insert(sig)
                })
                .collect();

            // Add assistant message with tool_calls to history
            self.history.append(ChatMessage {
                role: MessageRole::Assistant,
                content: response.content.clone(),
                name: None,
                tool_calls: Some(deduped_calls.clone()),
            });

            // ⑤ Tool dispatch
            for tool_call in &deduped_calls {
                let tool_name = &tool_call.function.name;
                let params_str = &tool_call.function.arguments;

                // Find the tool
                let tool = self.tools.iter().find(|t| {
                    let spec = t.spec();
                    spec.name == *tool_name
                });

                let result_content = match tool {
                    Some(tool) => {
                        // Execute the tool
                        let params: serde_json::Value = serde_json::from_str(params_str)
                            .unwrap_or(serde_json::Value::Object(Default::default()));

                        match tool.execute(params).await {
                            Ok(result) => {
                                if result.ok {
                                    result.content
                                } else {
                                    format!("Error: {}", result.error.unwrap_or_default())
                                }
                            }
                            Err(e) => format!("Tool execution error: {e}"),
                        }
                    }
                    None => format!("Unknown tool: {tool_name}"),
                };

                // ⑥ Append tool result to history
                let tool_result_message = ChatMessage {
                    role: MessageRole::Tool,
                    content: serde_json::json!({
                        "tool_call_id": tool_call.id,
                        "content": result_content,
                    })
                    .to_string(),
                    name: Some(tool_call.function.name.clone()),
                    tool_calls: None,
                };

                self.history.append(tool_result_message);

                // ⑧ Loop detection
                match self.loop_detector.check(
                    &tool_call.function.name,
                    &tool_call.function.arguments,
                    &result_content,
                ) {
                    LoopDetectionResult::NoLoop => {}
                    LoopDetectionResult::LoopDetected {
                        pattern: _,
                        level,
                        count: _,
                        message,
                    } => {
                        tracing::warn!(message = %message, level = ?level, "Loop detected");
                        match level {
                            ResponseLevel::Warning => {
                                self.history.append(ChatMessage {
                                    role: MessageRole::System,
                                    content: message,
                                    name: None,
                                    tool_calls: None,
                                });
                            }
                            ResponseLevel::Block => {
                                // Block was already handled by returning error as tool result
                            }
                            ResponseLevel::Break => {
                                return Err(RuntimeError::LoopDetected(message));
                            }
                        }
                    }
                }
            }

            // ⑦ Usage report (async, non-blocking) — Phase 1: just log
            // TODO(Phase 2): send UsageReport via IPC client
            tracing::debug!(iteration, "Usage report would be sent here (Phase 1: log only)");

            // ⑨ DevMode control
            // TODO(Phase 5): DevMode step control — debug.step(iteration)

            // Continue to next iteration
            tracing::debug!(iteration, "Loop iteration complete, continuing");
        }
    }

    /// Get reference to history manager
    pub fn history(&self) -> &HistoryManager {
        &self.history
    }

    /// Get mutable reference to history manager
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_loop_creation() {
        let _config = RuntimeConfig::default();
        // We can't easily test the full loop without a mock provider,
        // but we can verify construction
        let _budget = rollball_core::Budget {
            daily_tokens: Some(100000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "deny".to_string(),
        };
        // AgentLoop::new requires a real provider, tested via integration tests
    }
}
