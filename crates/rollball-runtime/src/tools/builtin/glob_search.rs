//! Glob search tool — search files by pattern

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use std::path::Path;

pub struct GlobSearchTool { work_dir: String }

impl GlobSearchTool {
    pub fn new(work_dir: &str) -> Self { Self { work_dir: work_dir.to_string() } }

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
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let pattern = params["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() { return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing 'pattern'".to_string()), token_usage: None }); }

        let base = Path::new(&self.work_dir);
        let mut results = Vec::new();

        fn walk_dir(dir: &Path, base: &Path, pattern: &str, results: &mut Vec<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk_dir(&path, base, pattern, results);
                    } else if let Some(rel) = path.strip_prefix(base).ok() {
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        if glob_match(pattern, &rel_str) {
                            results.push(rel_str);
                        }
                    }
                }
            }
        }

        walk_dir(base, base, pattern, &mut results);

        if results.is_empty() {
            Ok(ToolResult { ok: true, content: "No files matched the pattern".to_string(), error: None, token_usage: None })
        } else {
            Ok(ToolResult { ok: true, content: results.join("\n"), error: None, token_usage: None })
        }
    }
}

/// Simple glob matching — supports *, **, and ?
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    glob_match_parts(&pattern, &path_parts)
}

fn glob_match_parts(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() { return path.is_empty(); }
    if pattern[0] == "**" {
        for i in 0..=path.len() {
            if glob_match_parts(&pattern[1..], &path[i..]) { return true; }
        }
        return false;
    }
    if path.is_empty() { return false; }
    if part_match(pattern[0], path[0]) {
        glob_match_parts(&pattern[1..], &path[1..])
    } else {
        false
    }
}

fn part_match(pattern: &str, part: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = part.chars().collect();
    part_match_chars(&p, &s)
}

fn part_match_chars(p: &[char], s: &[char]) -> bool {
    if p.is_empty() { return s.is_empty(); }
    if s.is_empty() { return p.iter().all(|c| *c == '*'); }
    if p[0] == '?' || p[0] == s[0] { return part_match_chars(&p[1..], &s[1..]); }
    if p[0] == '*' {
        for i in 0..=s.len() {
            if part_match_chars(&p[1..], &s[i..]) { return true; }
        }
    }
    false
}
