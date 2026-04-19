//! File edit tool — precise string replacement in files

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct FileEditTool { work_dir: String }

impl FileEditTool {
    pub fn new(work_dir: &str) -> Self { Self { work_dir: work_dir.to_string() } }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_edit".to_string(),
            description: "Edit a file by replacing an exact string match with new content.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "old_text": { "type": "string", "description": "The exact text to find and replace" },
                    "new_text": { "type": "string", "description": "The replacement text" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("");
        let old_text = params["old_text"].as_str().unwrap_or("");
        let new_text = params["new_text"].as_str().unwrap_or("");

        if path.is_empty() || old_text.is_empty() {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing required parameters".to_string()), token_usage: None });
        }

        let full_path = Path::new(&self.work_dir).join(path);
        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to read file: {e}")), token_usage: None }),
        };

        let count = content.matches(old_text).count();
        if count == 0 {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some("old_text not found in file".to_string()), token_usage: None });
        }
        if count > 1 {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("old_text found {count} times — must be unique")), token_usage: None });
        }

        let new_content = content.replacen(old_text, new_text, 1);
        match tokio::fs::write(&full_path, &new_content).await {
            Ok(()) => Ok(ToolResult { ok: true, content: format!("Replaced in {path}"), error: None, token_usage: None }),
            Err(e) => Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to write file: {e}")), token_usage: None }),
        }
    }
}
