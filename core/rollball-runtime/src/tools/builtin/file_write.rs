//! File write tool — writes content to files within the workspace

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct FileWriteTool { work_dir: String }

impl FileWriteTool {
    pub fn new(work_dir: &str) -> Self { Self { work_dir: work_dir.to_string() } }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_write".to_string(),
            description: "Write content to a file. Creates the file if it doesn't exist.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("").trim_start_matches('/');
        let content = params["content"].as_str().unwrap_or("");
        if path.is_empty() { return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'path'".to_string()), token_usage: None }); }

        let full_path = Path::new(&self.work_dir).join(path);
        tracing::debug!(
            work_dir = %self.work_dir,
            input_path = %path,
            full_path = %full_path.display(),
            exists = full_path.exists(),
            "file_write: resolving path"
        );

        if let Some(parent) = full_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        match tokio::fs::write(&full_path, content).await {
            Ok(()) => Ok(ToolResult { ok: true, content: format!("Written {} bytes to {path}", content.len()), error: None, token_usage: None }),
            Err(e) => {
                tracing::warn!(
                    work_dir = %self.work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_write: failed to write file"
                );
                Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to write file: {e}")), token_usage: None })
            }
        }
    }
}
