//! File read tool — reads a line range (fragment) from a file within the workspace

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

use crate::tools::output;

const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const MAX_LINES_PER_CALL: usize = 200;

/// File read tool — fragment reader, not a whole-file reader
pub struct FileReadTool;

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_read".to_string(),
            description: "Read a specific range of lines from a file, with line numbers. This is a fragment reader — both start_line (1-based) and end_line (inclusive) are required. Read at most 200 lines per call; for longer ranges, paginate across multiple calls. Always use content_search first to locate the relevant line numbers before calling this tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "start_line": { "type": "integer", "description": "Starting line number (1-based). Required. Must be > 0." },
                    "end_line": { "type": "integer", "description": "Ending line number (inclusive). Required. Must be >= start_line. At most 200 lines per call — paginate if you need more." }
                },
                "required": ["path", "start_line", "end_line"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches('/');
        if path.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
                token_usage: None,
            });
        }

        // start_line and end_line are required
        let start_line_raw = match params["start_line"].as_u64() {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required 'start_line' parameter. Use content_search first to locate line numbers, then request a specific range (≤100 lines).".to_string()),
                    token_usage: None,
                });
            }
        };
        let end_line_raw = match params["end_line"].as_u64() {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required 'end_line' parameter".to_string()),
                    token_usage: None,
                });
            }
        };

        // Validate range
        if start_line_raw == 0 {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("start_line must be >= 1 (1-based)".to_string()),
                token_usage: None,
            });
        }
        if end_line_raw < start_line_raw {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "end_line ({end_line_raw}) must be >= start_line ({start_line_raw})"
                )),
                token_usage: None,
            });
        }
        let requested = (end_line_raw - start_line_raw + 1) as usize;
        if requested > MAX_LINES_PER_CALL {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Range too large: {requested} lines requested, max {MAX_LINES_PER_CALL} per call. Paginate across multiple calls (e.g. start_line: {s}, end_line: {e1}, then start_line: {e1p1}, end_line: {e2}...)",
                    s = start_line_raw,
                    e1 = start_line_raw + MAX_LINES_PER_CALL as u64 - 1,
                    e1p1 = start_line_raw + MAX_LINES_PER_CALL as u64,
                    e2 = end_line_raw,
                )),
                token_usage: None,
            });
        }

        let base = work_dir.unwrap_or(".");
        let full_path = Path::new(base).join(path);
        tracing::debug!(
            work_dir = %base,
            input_path = %path,
            full_path = %full_path.display(),
            exists = full_path.exists(),
            "file_read: resolving path"
        );

        // Check file size before reading to avoid loading huge files into memory
        match tokio::fs::metadata(&full_path).await {
            Ok(meta) => {
                if meta.len() > MAX_FILE_SIZE_BYTES {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "File too large: {} bytes (limit: {MAX_FILE_SIZE_BYTES} bytes)",
                            meta.len()
                        )),
                        token_usage: None,
                    });
                }
            }
            Err(e) => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to read file metadata: {e}")),
                    token_usage: None,
                });
            }
        }

        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let total = lines.len();

                if total == 0 {
                    return Ok(ToolResult {
                        ok: true,
                        content: "[File is empty]".to_string(),
                        error: None,
                        token_usage: None,
                    });
                }

                let s = (start_line_raw as usize).saturating_sub(1).min(total);
                let e = (end_line_raw as usize).min(total);

                if s >= e {
                    return Ok(ToolResult {
                        ok: true,
                        content: format!("[No lines in range, file has {total} lines]"),
                        error: None,
                        token_usage: None,
                    });
                }

                let numbered: String = lines[s..e]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {}", s + i + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");

                let summary = format!("\n[Lines {}-{} of {total}]", s + 1, e);

                let content = format!("{numbered}{summary}");
                let (content, _truncated) = output::truncate_output(&content);

                Ok(ToolResult {
                    ok: true,
                    content,
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => {
                tracing::warn!(
                    work_dir = %base,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_read: failed to read file"
                );
                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                    token_usage: None,
                })
            }
        }
    }
}
