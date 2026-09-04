//! Tool compression (ADR-052) was retired. The `context_retrieve` /
//! `context_abandon` tools are no longer registered with the LLM;
//! their source files survive in this module as dead code for future
//! reference. See `context_compression.rs` for the contract that used
//! to bind the two sides.
//!
//! `context_retrieve` (originally an LLM-initiated "re-expand
//! compressed tool result" tool) is kept here so the in-place restore
//! mechanics can be revisited if a future ADR re-introduces manual
//! tool-result recall.
#![allow(dead_code)]

//! ADR-052 context_retrieve tool - retrieve original tool result by tool_call_id.
//!
//! When a Tool message is compressed (replaced with a placeholder by
//! `context_abandon`), the LLM can call this tool to retrieve the
//! original full content.
//!
//! ## Resolution
//!
//! The tool searches the current session's JSONL conversation log for an entry
//! whose `metadata.tool_call_id` matches the requested ID.  This is an O(N)
//! scan over the JSONL file - acceptable because retrieve is explicitly
//! LLM-driven and happens at most a few times per session.
//!
//! ## Parameters
//!
//! - `tool_call_id` (string, required) - the `tool_call_id` embedded in the
//!   placeholder: `Call context_retrieve(id="toolu_xxx")`.
//!
//! ## In-place restore (ADR-052)
//!
//! ADR-032 used a transient channel so the recalled content was visible for
//! one LLM request only. ADR-052 replaces this with **in-place restore**: the
//! tool pushes `(tool_call_id, original_content)` into a `retrieve_queue`, and
//! the agent loop drains the queue on the next iteration, restoring the
//! original content at the placeholder's original position in history.
//! The tool itself returns only a short confirmation (~60 chars).
//!
//! This breaks the `recall -> compress -> recall` death loop that ADR-032's
//! transient mechanism was designed to prevent, because ADR-052 has removed
//! the automatic compression trigger entirely - the LLM must explicitly call
//! `context_abandon` to re-compress restored content.

use crate::agent::context_compression::RetrieveQueue;
use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use tracing;

/// ADR-052 context_retrieve tool for retrieving compressed tool results.
///
/// Unlike ADR-032's `context_recall` (which returned the full content via a
/// transient channel), this tool pushes the original content into a
/// `retrieve_queue` and returns only a short confirmation. The agent loop
/// restores the content in-place on the next iteration.
pub struct ContextRetrieveTool {
    /// Agent home directory (`config.work_dir`), parent of `conversations/`.
    agent_home: String,
    /// Shared queue for in-place restore. The tool writes here; the agent
    /// loop drains and calls `HistoryManager::retrieve_tool_result()`.
    retrieve_queue: RetrieveQueue,
}

impl ContextRetrieveTool {
    pub fn new(agent_home: &str, retrieve_queue: RetrieveQueue) -> Self {
        Self {
            agent_home: agent_home.to_string(),
            retrieve_queue,
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "context_retrieve".to_string(),
            description:
                "Retrieve the original full content of a compressed tool result by its tool_call_id. \
                 Call this when an earlier tool result was compressed to a placeholder and you need \
                 the complete output. \
                 \
                 The tool_call_id is embedded verbatim in every compressed placeholder: \
                 `[Tool result compressed. Call context_retrieve(id=\"toolu_xxx\") to retrieve the full content.]` \
                 Copy the id from the placeholder and pass it directly - no parsing needed."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_call_id": {
                        "type": "string",
                        "description": "The tool_call_id embedded in the compressed placeholder. \
                                        Example: if the placeholder says `context_retrieve(id=\"toolu_abc\")`, \
                                        pass `toolu_abc`."
                    }
                },
                "required": ["tool_call_id"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContextRetrieveTool {
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
                tracing::warn!("context_retrieve called with empty tool_call_id");
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(
                        "tool_call_id must be a non-empty string".to_string(),
                    ),
                    token_usage: None,
                })
            }
            None => {
                tracing::warn!("context_retrieve called without tool_call_id parameter");
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(
                        "Missing required parameter 'tool_call_id'".to_string(),
                    ),
                    token_usage: None,
                })
            }
        };

        // Search the conversations directory for a JSONL entry whose
        // metadata.tool_call_id matches.  We scan every line of every
        // session file under conversations/ - this is an O(total_sessions
        // × total_entries) scan, acceptable because:
        //   - context_retrieve is called rarely (only when LLM explicitly asks)
        //   - session files are small (typically < 500 entries)
        //   - the search stops at the first match per session, not scanning
        //     the entire file beyond the target entry
        let conversations_dir = Path::new(&self.agent_home).join("conversations");

        tracing::info!(
            conversations_dir = %conversations_dir.display(),
            tool_call_id = %tool_call_id,
            "context_retrieve invoked"
        );
        if !conversations_dir.exists() {
            tracing::warn!(
                conversations_dir = %conversations_dir.display(),
                "context_retrieve: conversations dir not found"
            );
            return Ok(ToolResult {
                ok: true,
                content: format!(
                    "No conversations directory found at {}. \
                     This is normal for a brand-new agent with no conversation history.",
                    conversations_dir.display()
                ),
                error: None,
                token_usage: None,
            });
        }

        let mut entries = match std::fs::read_dir(&conversations_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(
                    dir = %conversations_dir.display(),
                    error = %e,
                    "context_retrieve: failed to read conversations dir"
                );
                return Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Could not read conversations directory: {}",
                        e
                    ),
                    error: None,
                    token_usage: None,
                })
            }
        };

        let mut files_found: Vec<std::path::PathBuf> = Vec::new();
        while let Some(Ok(entry)) = entries.next() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                files_found.push(path);
            }
        }
        // Sort by modification time (newest first) so we check the most
        // recent session first - the current session is most likely
        // to contain the matching result.
        files_found.sort_by(|a, b| {
            b.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                .cmp(
                    &a.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                )
        });

        tracing::info!(
            files_count = files_found.len(),
            files = ?files_found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "context_retrieve: searching conversation logs"
        );

        for file_path in &files_found {
            tracing::debug!(
                file = %file_path.display(),
                "context_retrieve: checking file"
            );
            if let Some(original_content) = find_in_jsonl(file_path, &tool_call_id) {
                // ADR-052: Push (tool_call_id, original_content) into the
                // retrieve_queue. The agent loop will drain this on the next
                // iteration and restore the original content in-place.
                let content_len = original_content.len();
                self.retrieve_queue
                    .lock()
                    .unwrap()
                    .push_back((tool_call_id.clone(), original_content));

                tracing::info!(
                    tool_call_id = %tool_call_id,
                    content_len,
                    "context_retrieve: match found, queued for in-place restore"
                );

                return Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "Retrieved {} ({} chars), original content restored.",
                        tool_call_id, content_len
                    ),
                    error: None,
                    token_usage: None,
                });
            }
        }

        tracing::warn!(
            tool_call_id = %tool_call_id,
            "context_retrieve: not found in any session file"
        );

        Ok(ToolResult {
            ok: false,
            content: String::new(),
            error: Some(format!(
                "No tool result found with tool_call_id '{}' in any session file. \
                 Verify the tool_call_id matches exactly (including any prefix like \
                 'toolu_' or 'call_').",
                tool_call_id
            )),
            token_usage: None,
        })
    }
}

/// Scan a single JSONL file for an entry with matching `metadata.tool_call_id`.
/// Returns `Some(original_content)` if found, `None` otherwise.
fn find_in_jsonl(path: &Path, target_id: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check metadata.tool_call_id
        let tc_id = entry
            .get("metadata")
            .and_then(|m| m.get("tool_call_id"))
            .and_then(|v| v.as_str());
        match tc_id {
            Some(id) if id == target_id => {
                let role = entry
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if role != "tool_result" {
                    // Entry exists but role is wrong - keep scanning.
                    // The actual tool_result entry might be further down.
                    tracing::debug!(
                        file = %path.display(),
                        target_id = %target_id,
                        role = %role,
                        "context_retrieve: found matching tool_call_id but wrong role"
                    );
                    continue;
                }
                // Found the actual tool_result - return its content.
                let result_content = entry
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                tracing::info!(
                    file = %path.display(),
                    target_id = %target_id,
                    content_len = result_content.len(),
                    "context_retrieve: match found"
                );
                return Some(result_content);
            }
            _ => continue,
        }
    }

    tracing::debug!(
        file = %path.display(),
        target_id = %target_id,
        "context_retrieve: not found in this file"
    );

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a single JSONL entry to a temp file.
    fn write_entry(file: &mut impl std::io::Write, role: &str, content: &str, id: &str) {
        let entry = serde_json::json!({
            "role": role,
            "content": content,
            "metadata": {
                "tool_call_id": id
            }
        });
        writeln!(file, "{}", entry).unwrap();
    }

    /// Helper: create a RetrieveQueue for testing.
    fn test_queue() -> RetrieveQueue {
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
    }

    #[test]
    fn test_find_in_jsonl_matches() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "user", "Hello", "");
        write_entry(&mut tmp, "tool_result", "This is the hidden content", "toolu_abc");
        write_entry(&mut tmp, "assistant", "Done", "");

        let result = find_in_jsonl(tmp.path(), "toolu_abc").unwrap();
        assert_eq!(result, "This is the hidden content");
    }

    #[test]
    fn test_find_in_jsonl_not_found() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "tool_result", "Some content", "toolu_xyz");

        let result = find_in_jsonl(tmp.path(), "toolu_nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_in_jsonl_skips_tool_call_finds_tool_result() {
        // Regression test: when both a tool_call (role="tool_call") and a
        // tool_result share the same tool_call_id, and the tool_call appears
        // first in the file, find_in_jsonl must skip the tool_call entry and
        // continue scanning to find the tool_result.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "tool_call", "", "toolu_shared_id");
        write_entry(&mut tmp, "user", "What was the result?", "");
        write_entry(&mut tmp, "tool_result", "The actual tool result content", "toolu_shared_id");

        let result = find_in_jsonl(tmp.path(), "toolu_shared_id").unwrap();
        assert_eq!(
            result, "The actual tool result content",
            "Should return the tool_result content, not a role error"
        );
    }

    #[test]
    fn test_tool_missing_param() {
        let tool = ContextRetrieveTool::new("/tmp", test_queue());
        let params = serde_json::json!({});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[test]
    fn test_tool_empty_param() {
        let tool = ContextRetrieveTool::new("/tmp", test_queue());
        let params = serde_json::json!({"tool_call_id": ""});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("non-empty"));
    }

    #[test]
    fn test_retrieve_queue_pushed_on_match() {
        // ADR-052: when find_in_jsonl succeeds, the tool should push
        // (tool_call_id, original_content) into the retrieve_queue and
        // return a short confirmation (not the full content).
        let dir = tempfile::tempdir().unwrap();
        let conversations = dir.path().join("conversations");
        std::fs::create_dir(&conversations).unwrap();
        let jsonl = conversations.join("test.jsonl");
        std::fs::write(&jsonl, serde_json::json!({
            "role": "tool_result",
            "content": "The hidden content",
            "metadata": {"tool_call_id": "toolu_test"}
        }).to_string() + "\n").unwrap();

        let queue = test_queue();
        let tool = ContextRetrieveTool::new(&dir.path().to_string_lossy(), queue.clone());
        let params = serde_json::json!({"tool_call_id": "toolu_test"});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();

        assert!(result.ok);
        // Tool returns short confirmation, NOT the full content
        assert!(result.content.contains("Retrieved toolu_test"));
        assert!(result.content.contains("chars"));
        assert!(!result.content.contains("The hidden content"));

        // Queue should have the entry
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), 1);
        let (id, content) = &q[0];
        assert_eq!(id, "toolu_test");
        assert_eq!(content, "The hidden content");
    }
}
