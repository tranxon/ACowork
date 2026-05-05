//! Glob search tool — search files by pattern using ripgrep's ignore crate

use async_trait::async_trait;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct GlobSearchTool {
    work_dir: String,
}

impl GlobSearchTool {
    pub fn new(work_dir: &str) -> Self {
        Self {
            work_dir: work_dir.to_string(),
        }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "glob_search".to_string(),
            description: "Search for files matching a glob pattern (e.g., '**/*.rs')".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern to match files" }
                },
                "required": ["pattern"]
            }),
        }
    }
}

#[async_trait]
impl Tool for GlobSearchTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let pattern = params["pattern"]
            .as_str()
            .unwrap_or("")
            .replace('\\', "/");

        if pattern.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'pattern'".to_string()),
                token_usage: None,
            });
        }

        let base = Path::new(&self.work_dir);

        // Build glob override so WalkBuilder only yields matching files
        let mut override_builder = OverrideBuilder::new(base);
        if let Err(e) = override_builder.add(&pattern) {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Invalid glob pattern: {e}")),
                token_usage: None,
            });
        }
        let overrides = match override_builder.build() {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Invalid glob pattern: {e}")),
                    token_usage: None,
                });
            }
        };

        let mut results = Vec::new();
        let walker = WalkBuilder::new(base)
            .overrides(overrides)
            .hidden(true) // respect hidden files setting (default)
            .git_ignore(true) // respect .gitignore
            .git_global(true) // respect global gitignore
            .git_exclude(true) // respect .git/info/exclude
            .build();

        for entry in walker {
            match entry {
                Ok(e) => {
                    // Only collect files, skip directories
                    if e.file_type().is_some_and(|ft| ft.is_file())
                        && let Ok(rel) = e.path().strip_prefix(base)
                    {
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        results.push(rel_str);
                    }
                }
                Err(_) => continue,
            }
        }

        if results.is_empty() {
            Ok(ToolResult {
                ok: true,
                content: "No files matched the pattern".to_string(),
                error: None,
                token_usage: None,
            })
        } else {
            Ok(ToolResult {
                ok: true,
                content: results.join("\n"),
                error: None,
                token_usage: None,
            })
        }
    }
}
