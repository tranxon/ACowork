//! Content search tool — regex search in file contents

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct ContentSearchTool { work_dir: String }

impl ContentSearchTool {
    pub fn new(work_dir: &str) -> Self { Self { work_dir: work_dir.to_string() } }
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "content_search".to_string(),
            description: "Search file contents using a regex pattern. Returns matching lines with file paths.".to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": { "pattern": { "type": "string", "description": "Regex pattern to search for" }, "path": { "type": "string", "description": "Optional subdirectory to search in" } }, "required": ["pattern"] }),
        }
    }
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let pattern = params["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() { return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'pattern'".to_string()), token_usage: None }); }

        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Invalid regex: {e}")), token_usage: None }),
        };

        let search_dir = params["path"].as_str().map(|p| Path::new(&self.work_dir).join(p)).unwrap_or_else(|| Path::new(&self.work_dir).to_path_buf());
        let mut results = Vec::new();

        fn walk_and_search(dir: &Path, base: &Path, re: &regex::Regex, results: &mut Vec<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() { walk_and_search(&path, base, re, results); continue; }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        for (i, line) in content.lines().enumerate() {
                            if re.is_match(line) {
                                let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy();
                                results.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
                                if results.len() >= 50 { return; }
                            }
                        }
                    }
                }
            }
        }

        walk_and_search(&search_dir, Path::new(&self.work_dir), &re, &mut results);
        Ok(ToolResult { ok: true, content: if results.is_empty() { "No matches found".to_string() } else { results.join("\n") }, error: None, token_usage: None })
    }
}
