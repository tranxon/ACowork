//! File read tool — reads files within the workspace

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

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
            description: "Read the contents of a file. Supports optional line range.".to_string(),
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
        let path = params["path"].as_str().unwrap_or("");
        if path.is_empty() {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'path' parameter".to_string()), token_usage: None });
        }

        let full_path = Path::new(&self.work_dir).join(path);
        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => {
                // Handle line range if specified
                let start = params["start_line"].as_u64().unwrap_or(1) as usize;
                let end = params["end_line"].as_u64().unwrap_or(0) as usize;

                let result = if start > 0 || end > 0 {
                    let lines: Vec<&str> = content.lines().collect();
                    let s = if start > 0 { start - 1 } else { 0 };
                    let e = if end > 0 { end.min(lines.len()) } else { lines.len() };
                    if s < lines.len() {
                        lines[s..e].join("\n")
                    } else {
                        String::new()
                    }
                } else {
                    content
                };

                Ok(ToolResult { ok: true, content: result, error: None, token_usage: None })
            }
            Err(e) => Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to read file: {e}")), token_usage: None }),
        }
    }
}
