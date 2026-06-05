//! User interaction handling for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 4).
//! Contains methods for "special tools" that intercept the normal tool
//! dispatch flow and involve user interaction sub-protocols:
//! - `handle_ask_user_question`: validates params, emits AskQuestion event,
//!   transitions to WaitingApproval, blocks until user answers
//! - `handle_todo_write`: updates session todo list and injects into context
//!
//! These methods are independent of the main loop orchestration — they are
//! called from the tool dispatch step in execute_single_iteration when a
//! matching tool call is detected.

use rollball_core::providers::traits::ToolCall;

use crate::agent::context::ContextBuilder;
use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session_state::SessionStatus;
use crate::tools::builtin::ask_user_question::AskUserQuestionTool;

impl AgentLoop {
    /// Handle an `ask_user_question` tool call.
    ///
    /// Validates the params, emits ChunkEvent::AskQuestion, transitions
    /// status to WaitingApproval, and blocks until the user responds.
    /// Returns the user's answer as a tool result string.
    pub(crate) async fn handle_ask_user_question(&mut self, tc: &ToolCall) -> String {
        // Validate params
        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: ask_user_question arguments are not valid JSON: {}", e);
            }
        };

        let parsed = match AskUserQuestionTool::validate_params(&params) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: ask_user_question invalid params: {}", e);
            }
        };

        // Generate unique request ID
        let request_id = format!(
            "q-{}",
            self.approval_next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        tracing::info!(
            request_id = %request_id,
            question = %parsed.question,
            options_count = parsed.options.len(),
            "AskUserQuestion: emitting AskQuestion event and waiting for answer"
        );

        // Emit ChunkEvent::AskQuestion
        let _ = self.core.try_send_chunk(ChunkEvent::AskQuestion {
            request_id: request_id.clone(),
            question: parsed.question.clone(),
            options: parsed.options,
            title: parsed.title.clone(),
            timeout_seconds: parsed.timeout_seconds,
        });

        // Transition to WaitingApproval
        self.transition_status(SessionStatus::WaitingApproval {
            request_id: request_id.clone(),
        });

        // Wait for the user's answer (with optional timeout)
        let answer = self.await_question_answer(&request_id, parsed.timeout_seconds).await;

        // Transition back to Streaming (the loop will continue)
        self.transition_status(SessionStatus::Streaming { message_id: None });

        tracing::info!(
            request_id = %request_id,
            answer_preview = %answer.chars().take(100).collect::<String>(),
            "AskUserQuestion: received answer"
        );

        // Return the answer as the tool result
        answer
    }

    /// Handle a `todo_write` tool call by updating SessionState.todos and
    /// injecting the updated list into the ContextBuilder for the next build().
    ///
    /// This is synchronous (no I/O or user interaction) since todos are
    /// pure in-memory state on SessionState.
    pub(crate) fn handle_todo_write(
        &mut self,
        tc: &ToolCall,
        context_builder: &mut ContextBuilder,
    ) -> String {
        use crate::agent::session_state::TodoItem;

        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: todo_write arguments are not valid JSON: {}", e);
            }
        };

        let todos_array = match params.get("todos").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return "Error: todo_write requires a 'todos' array parameter".to_string(),
        };

        let merge = params
            .get("merge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut items: Vec<TodoItem> = Vec::with_capacity(todos_array.len());
        for item in todos_array {
            let id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return "Error: each todo item must have a string 'id' field".to_string(),
            };
            let content = match item.get("content").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return format!("Error: todo item '{}' missing required 'content' field", id)
                }
            };
            let status = match item.get("status").and_then(|v| v.as_str()) {
                Some("pending") => crate::agent::session_state::TodoStatus::Pending,
                Some("in_progress") => crate::agent::session_state::TodoStatus::InProgress,
                Some("completed") => crate::agent::session_state::TodoStatus::Completed,
                Some(other) => {
                    return format!(
                        "Error: todo item '{}' has invalid status '{}'. Must be one of: pending, in_progress, completed",
                        id, other
                    )
                }
                None => {
                    return format!("Error: todo item '{}' missing required 'status' field", id)
                }
            };
            items.push(TodoItem {
                id,
                content,
                status,
            });
        }

        // Update the session todos
        self.session.update_todos(items, merge);

        // Inject the updated list into context builder for the next build()
        context_builder.set_todo_context(self.session.format_todos());

        // Emit TodoListUpdated event to frontend for UI rendering
        let _ = self.core.try_send_chunk(ChunkEvent::TodoListUpdated {
            todos: self.session.todos.clone(),
        });

        // Return formatted list as the tool result
        match self.session.format_todos() {
            Some(formatted) => {
                let count = self.session.todos.len();
                format!(
                    "Todo list updated ({} items, merge={}):\n{}",
                    count, merge, formatted
                )
            }
            None => "Todo list is now empty.".to_string(),
        }
    }
}
