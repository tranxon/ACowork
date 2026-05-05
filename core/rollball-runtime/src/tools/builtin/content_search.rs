//! Content search tool — regex search in file contents using ripgrep's ignore crate

use async_trait::async_trait;
use ignore::WalkBuilder;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct ContentSearchTool {
    work_dir: String,
}

impl ContentSearchTool {
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: work_dir.to_string(),
        }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "content_search".to_string(),
            description: "Search file contents using a regex pattern. Returns matching lines with file paths.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Optional subdirectory to search in" }
                },
                "required": ["pattern"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContentSearchTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let pattern = params["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'pattern'".to_string()),
                token_usage: None,
            });
        }

        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Invalid regex: {e}")),
                    token_usage: None,
                });
            }
        };

        let base = Path::new(&self.work_dir);
        let search_dir = params["path"]
            .as_str()
            .map(|p| base.join(p))
            .unwrap_or_else(|| base.to_path_buf());

        let mut results = Vec::new();
        let walker = WalkBuilder::new(&search_dir)
            .hidden(true) // respect hidden files setting (default)
            .git_ignore(true) // respect .gitignore
            .git_global(true) // respect global gitignore
            .git_exclude(true) // respect .git/info/exclude
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Skip directories
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();
            // Read file content; binary files will fail read_to_string and are skipped
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    let rel = path.strip_prefix(base).unwrap_or(path).to_string_lossy();
                    let rel_str = rel.replace('\\', "/");
                    results.push(format!("{}:{}: {}", rel_str, i + 1, line.trim()));
                    if results.len() >= 50 {
                        break;
                    }
                }
            }

            if results.len() >= 50 {
                break;
            }
        }

        Ok(ToolResult {
            ok: true,
            content: if results.is_empty() {
                "No matches found".to_string()
            } else {
                results.join("\n")
            },
            error: None,
            token_usage: None,
        })
    }
}
