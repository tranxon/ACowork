//! Glob search tool — search files by pattern using globset for matching

use async_trait::async_trait;
use ignore::WalkBuilder;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

use crate::tools::output;
use crate::tools::path_utils;
use crate::tools::workspace_resolver::WorkspaceResolver;

pub struct GlobSearchTool {
    resolver: WorkspaceResolver,
}

impl GlobSearchTool {
    pub fn new(resolver: &WorkspaceResolver) -> Self {
        Self {
            resolver: resolver.clone(),
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
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_string();

        if pattern.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'pattern'".to_string()),
                token_usage: None,
            });
        }

        let glob_set = match path_utils::build_glob_set(&pattern, false) {
            Ok(gs) => gs,
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
        let mut truncated = false;

        // Search current workspace only (respecting workspace setting)
        let search_base = Path::new(self.resolver.current_dir());
        if !search_base.exists() {
            return Ok(ToolResult {
                ok: true,
                content: "No files matched the pattern".to_string(),
                error: None,
                token_usage: None,
            });
        }

        let walker = WalkBuilder::new(search_base)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker {
            match entry {
                Ok(e) => {
                    if e.file_type().is_some_and(|ft| ft.is_file()) {
                        let path = e.path();
                        let rel_str =
                            path_utils::normalize_separators(&path_utils::relative_path(path, search_base));
                        if glob_set.is_match(&rel_str) {
                            if results.len() >= output::MAX_RESULT_COUNT {
                                truncated = true;
                                break;
                            }
                            if !results.iter().any(|r: &String| r.eq_ignore_ascii_case(&rel_str)) {
                                results.push(rel_str);
                            }
                        }
                    }
                }
                Err(_) => continue,
            }
            if truncated {
                break;
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
            let content = results.join("\n");
            let mut content = if truncated {
                format!(
                    "{content}\n\n[Results truncated: showing first {} results]",
                    output::MAX_RESULT_COUNT
                )
            } else {
                content
            };
            content.push_str(&format!("\n\nTotal: {} files", results.len()));

            let (content, _truncated) = output::truncate_output(&content);
            Ok(ToolResult {
                ok: true,
                content,
                error: None,
                token_usage: None,
            })
        }
    }
}
