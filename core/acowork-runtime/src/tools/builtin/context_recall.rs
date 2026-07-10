//! ADR-032 context_recall tool — retrieve original tool result by tool_call_id.
//!
//! When a Tool message is compressed by `HistoryManager::compress_tool_results`,
//! its content is replaced with a ~120-char placeholder. The LLM can call this
//! tool to retrieve the original full content.
//!
//! ## Resolution
//!
//! The tool searches the current session's JSONL conversation log for an entry
//! whose `metadata.tool_call_id` matches the requested ID.  This is an O(N)
//! scan over the JSONL file — acceptable because recall is explicitly LLM-driven
//! and happens at most a few times per session.
//!
//! ## Parameters
//!
//! - `tool_call_id` (string, required) — the `tool_call_id` embedded in the
//!   placeholder: `Call context_recall(id="toolu_xxx")`.
//!
//! ## Transient (planned)
//!
//! C3a will mark this tool's result as `transient: true` so the recalled content
//! is injected into the next LLM request without permanently appending to history.
//! For now (C3b baseline), the result is appended to history as a regular Tool
//! message — which is correct but wastes context if the LLM does not immediately
//! use the content.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

/// ADR-032 context_recall tool for retrieving compressed tool results.
pub struct ContextRecallTool {
    /// Agent home directory (`config.work_dir`), parent of `conversations/`.
    agent_home: String,
}

impl ContextRecallTool {
    pub fn new(agent_home: &str) -> Self {
        Self {
            agent_home: agent_home.to_string(),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "context_recall".to_string(),
            description:
                "Retrieve the original full content of a compressed tool result by its tool_call_id. \
                 Call this when an earlier tool result was compressed to a placeholder and you need \
                 the complete output. The tool_call_id is included in every compressed placeholder: \
                 `[Tool result compressed. Call context_recall(id=\"toolu_xxx\")...]{.open}"
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_call_id": {
                        "type": "string",
                        "description": "The tool_call_id embedded in the compressed placeholder. \
                                        Example: if the placeholder says `context_recall(id=\"toolu_abc\")`, \
                                        pass `toolu_abc`."
                    }
                },
                "required": ["tool_call_id"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContextRecallTool {
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
        // session file under conversations/ — this is an O(total_sessions
        // × total_entries) scan, acceptable because:
        //   - context_recall is called rarely (only when LLM explicitly asks)
        //   - session files are small (typically < 500 entries)
        //   - the search stops at the first match per session, not scanning
        //     the entire file beyond the target entry
        let conversations_dir = Path::new(&self.agent_home).join("conversations");
        if !conversations_dir.exists() {
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
        // recent session first — the current session is most likely
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

        for file_path in &files_found {
            if let Some(result) = find_in_jsonl(file_path, &tool_call_id) {
                return Ok(result);
            }
        }

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
/// Returns `None` if no match is found.
fn find_in_jsonl(path: &Path, target_id: &str) -> Option<ToolResult> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: serde_json::Value = serde_json::from_str(trimmed).ok()?;

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
                    // Entry exists but is not a tool_result — return
                    // a friendly message instead of "not found".
                    return Some(ToolResult {
                        ok: true,
                        content: format!(
                            "Found entry with tool_call_id '{}' but its role is '{}', not 'tool_result'. \
                             Only tool_result entries have content that can be recalled.",
                            target_id,
                            role
                        ),
                        error: None,
                        token_usage: None,
                    });
                }
                let result_content = entry
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some(ToolResult {
                    ok: true,
                    content: result_content,
                    error: None,
                    token_usage: None,
                });
            }
            _ => continue,
        }
    }
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

    #[test]
    fn test_find_in_jsonl_matches() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "user", "Hello", "");
        write_entry(&mut tmp, "tool_result", "This is the hidden content", "toolu_abc");
        write_entry(&mut tmp, "assistant", "Done", "");

        let result = find_in_jsonl(tmp.path(), "toolu_abc").unwrap();
        assert!(result.ok);
        assert_eq!(result.content, "This is the hidden content");
        assert!(result.error.is_none());
    }

    #[test]
    fn test_find_in_jsonl_not_found() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "tool_result", "Some content", "toolu_xyz");

        let result = find_in_jsonl(tmp.path(), "toolu_nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_in_jsonl_wrong_role() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write_entry(&mut tmp, "assistant", "Some assistant text", "toolu_assistant_id");

        let result = find_in_jsonl(tmp.path(), "toolu_assistant_id").unwrap();
        assert!(result.ok);
        assert!(result.content.contains("role is 'assistant'"));
    }

    #[test]
    fn test_tool_missing_param() {
        let tool = ContextRecallTool::new("/tmp");
        let params = serde_json::json!({});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[test]
    fn test_tool_empty_param() {
        let tool = ContextRecallTool::new("/tmp");
        let params = serde_json::json!({"tool_call_id": ""});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("non-empty"));
    }
}
