//! ADR-052 context_abandon tool - replace a tool result with a placeholder.
//!
//! When the LLM determines a tool result is no longer needed for current
//! reasoning, it calls this tool to replace the in-memory content with a
//! compact placeholder. The original content is preserved in the JSONL
//! conversation log and can be retrieved later via `context_retrieve`.
//!
//! ## Execution model
//!
//! This tool does **not** directly modify `HistoryManager` (tools have no
//! access to it). Instead, it pushes the `tool_call_id` into an
//! `abandon_queue`. The agent loop drains the queue on the next iteration
//! and calls `HistoryManager::abandon_tool_result()` to perform the
//! in-place replacement. This is symmetric with `context_retrieve`'s
//! `retrieve_queue` design.
//!
//! ## Parameters
//!
//! - `tool_call_id` (string, required) - the `tool_call_id` of the tool
//!   result to abandon. This is the same id that appears in tool results
//!   and in compressed placeholders.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tracing;

/// ADR-052: Type alias for the abandon queue shared between the tool and the
/// agent loop. The tool writes `tool_call_id` strings here; the agent loop
/// drains them and calls `HistoryManager::abandon_tool_result()`.
pub type AbandonQueue = Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

/// ADR-052 context_abandon tool for replacing tool results with placeholders.
///
/// The tool pushes the `tool_call_id` into a shared queue. The agent loop
/// drains the queue and performs the actual in-place replacement via
/// `HistoryManager::abandon_tool_result()`.
pub struct ContextAbandonTool {
    /// Shared queue for abandon requests.
    abandon_queue: AbandonQueue,
}

impl ContextAbandonTool {
    pub fn new(abandon_queue: AbandonQueue) -> Self {
        Self { abandon_queue }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "context_abandon".to_string(),
            description:
                "Replace a tool result with a compact placeholder to free up context window space. \
                 The original content is preserved in the conversation log and can be retrieved \
                 later with context_retrieve. Call this when a tool result is no longer needed \
                 for your current reasoning - e.g., after you've extracted the relevant \
                 information from a large file_read or content_search output."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_call_id": {
                        "type": "string",
                        "description": "The tool_call_id of the tool result to abandon. \
                                        This is the same id that appears in tool results and \
                                        in compressed placeholders."
                    }
                },
                "required": ["tool_call_id"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContextAbandonTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let tool_call_id = match params.get("tool_call_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            Some(_) => {
                tracing::warn!("context_abandon called with empty tool_call_id");
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("tool_call_id must be a non-empty string".to_string()),
                    token_usage: None,
                });
            }
            None => {
                tracing::warn!("context_abandon called without tool_call_id parameter");
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'tool_call_id'".to_string()),
                    token_usage: None,
                });
            }
        };

        // Push the tool_call_id into the abandon_queue. The agent loop
        // will drain this on the next iteration and call
        // `HistoryManager::abandon_tool_result()` to replace the content
        // in-place with a placeholder.
        self.abandon_queue
            .lock()
            .unwrap()
            .push_back(tool_call_id.clone());

        tracing::info!(
            tool_call_id = %tool_call_id,
            "context_abandon: queued for in-place replacement"
        );

        Ok(ToolResult {
            ok: true,
            content: format!(
                "Tool result '{}' will be replaced with a placeholder.",
                tool_call_id
            ),
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create an AbandonQueue for testing.
    fn test_queue() -> AbandonQueue {
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
    }

    #[test]
    fn test_tool_missing_param() {
        let tool = ContextAbandonTool::new(test_queue());
        let params = serde_json::json!({});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[test]
    fn test_tool_empty_param() {
        let tool = ContextAbandonTool::new(test_queue());
        let params = serde_json::json!({"tool_call_id": ""});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("non-empty"));
    }

    #[test]
    fn test_abandon_queue_pushed() {
        let queue = test_queue();
        let tool = ContextAbandonTool::new(queue.clone());
        let params = serde_json::json!({"tool_call_id": "toolu_abc"});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();

        assert!(result.ok);
        assert!(result.content.contains("toolu_abc"));
        assert!(result.content.contains("placeholder"));

        // Queue should have the entry
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0], "toolu_abc");
    }

    #[test]
    fn test_tool_name_and_spec() {
        let tool = ContextAbandonTool::new(test_queue());
        let spec = tool.spec();
        assert_eq!(spec.name, "context_abandon");
        assert!(spec.description.contains("placeholder"));
        assert!(spec.description.contains("context_retrieve"));
    }
}
