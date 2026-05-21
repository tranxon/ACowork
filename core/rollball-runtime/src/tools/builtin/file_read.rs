//! File read tool — reads files within the workspace

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

use crate::tools::output;

const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// File read tool
pub struct FileReadTool {
    work_dir: String,
}

impl FileReadTool {
    pub fn new(work_dir: &str) -> Self {
        Self { work_dir: work_dir.to_string() }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_read".to_string(),
            description: "Read the contents of a file with line numbers. Supports optional line range via start_line (1-based) and end_line (inclusive).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "start_line": { "type": "integer", "description": "Optional start line (1-based)" },
                    "end_line": { "type": "integer", "description": "Optional end line (inclusive)" }
                },
                "required": ["path"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("").trim_start_matches('/');
        if path.is_empty() {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'path' parameter".to_string()), token_usage: None });
        }

        let full_path = Path::new(&self.work_dir).join(path);
        tracing::debug!(
            work_dir = %self.work_dir,
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
                    return Ok(ToolResult { ok: true, content: String::new(), error: None, token_usage: None });
                }

                let s = params["start_line"].as_u64()
                    .map(|v| (v.max(1) as usize).saturating_sub(1))
                    .unwrap_or(0)
                    .min(total);
                let e = params["end_line"].as_u64()
                    .map(|v| (v as usize).min(total))
                    .unwrap_or(total);

                // end_line defaults to 0 (meaning read to end) — clamp
                let e = if e == 0 { total } else { e };

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

                let partial = s > 0 || e < total;
                let summary = if partial {
                    format!("\n[Lines {}-{} of {total}]", s + 1, e)
                } else {
                    format!("\n[{total} lines total]")
                };

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
                    work_dir = %self.work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_read: failed to read file"
                );
                Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to read file: {e}")), token_usage: None })
            }
        }
    }
}
