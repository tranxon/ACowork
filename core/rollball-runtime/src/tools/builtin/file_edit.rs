//! File edit tool — precise string replacement in files
//!
//! Matching strategy (in order):
//! 1. Exact string match — fast path, preferred
//! 2. Whitespace-flexible line matching — normalizes whitespace differences
//!    (indentation, trailing spaces, tab/space mixing) to handle LLM-generated
//!    old_text that doesn't exactly match file content.

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
            description: "Edit a file by replacing an exact string match with new content. Exact matching is preferred; if exact matching fails, whitespace-flexible line matching is attempted.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "old_text": { "type": "string", "description": "The exact text to find and replace. Whose match is tried first; if no exact match, you need to try whitespace-flexible matching." },
                    "new_text": { "type": "string", "description": "The replacement text" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Matching helpers
// ---------------------------------------------------------------------------

/// Normalize whitespace in a line for flexible comparison:
/// - Trim trailing spaces/tabs
/// - Collapse runs of spaces/tabs to a single space
fn normalize_line(line: &str) -> String {
    let trimmed = line.trim_end_matches([' ', '\t']);
    let mut normalized = String::with_capacity(trimmed.len());
    let mut in_whitespace_run = false;

    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            if !in_whitespace_run {
                normalized.push(' ');
                in_whitespace_run = true;
            }
        } else {
            normalized.push(ch);
            in_whitespace_run = false;
        }
    }

    normalized
}

/// Byte ranges of each line in the content.
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,       // byte offset of first character
    content_end: usize, // byte offset after last non-line-terminator character
    end: usize,         // byte offset after line terminator (or content_end for last line)
}

fn compute_line_spans(content: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let bytes = content.as_bytes();
    let mut line_start = 0usize;

    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            let mut content_end = idx;
            if content_end > line_start && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            spans.push(LineSpan {
                start: line_start,
                content_end,
                end: idx + 1,
            });
            line_start = idx + 1;
        }
    }

    if line_start < content.len() {
        spans.push(LineSpan {
            start: line_start,
            content_end: content.len(),
            end: content.len(),
        });
    }

    spans
}

/// Match outcome with start/end byte offsets and a flag indicating whether
/// whitespace-flexible matching was used.
#[derive(Debug, Clone, Copy)]
struct MatchOutcome {
    start: usize,
    end: usize,
    used_whitespace_flex: bool,
}

/// Try whitespace-flexible line matching.
///
/// When exact matching fails, this function normalizes whitespace in both the
/// old_string and file content, then tries to find a unique match line-by-line.
fn try_flexible_line_match(content: &str, old_string: &str) -> Result<MatchOutcome, String> {
    let content_spans = compute_line_spans(content);
    let old_spans = compute_line_spans(old_string);

    if old_spans.is_empty() || content_spans.len() < old_spans.len() {
        return Err("old_text not found in file".into());
    }

    let normalized_old_lines: Vec<String> = old_spans
        .iter()
        .map(|span| normalize_line(&old_string[span.start..span.content_end]))
        .collect();
    let normalized_content_lines: Vec<String> = content_spans
        .iter()
        .map(|span| normalize_line(&content[span.start..span.content_end]))
        .collect();

    let mut match_count = 0usize;
    let mut matched_start_line = 0usize;
    let window_size = old_spans.len();

    for start_line in 0..=(content_spans.len() - window_size) {
        let mut window_matches = true;
        for line_offset in 0..window_size {
            if normalized_content_lines[start_line + line_offset]
                != normalized_old_lines[line_offset]
            {
                window_matches = false;
                break;
            }
        }

        if window_matches {
            match_count += 1;
            if match_count == 1 {
                matched_start_line = start_line;
            }
        }
    }

    if match_count == 0 {
        return Err("old_text not found in file".into());
    }

    if match_count > 1 {
        return Err(format!(
            "old_text matches {match_count} times with whitespace flexibility; must match exactly once"
        ));
    }

    let first_span = content_spans[matched_start_line];
    let last_span = content_spans[matched_start_line + window_size - 1];
    let end = if old_string.ends_with('\n') {
        last_span.end
    } else {
        last_span.content_end
    };

    Ok(MatchOutcome {
        start: first_span.start,
        end,
        used_whitespace_flex: true,
    })
}

/// Resolve a match in content for old_string.
///
/// Tries exact string matching first. If no exact match is found, falls back
/// to whitespace-flexible line matching.
fn resolve_match(content: &str, old_string: &str) -> Result<MatchOutcome, String> {
    // 1. Exact match
    let mut exact_matches = content.match_indices(old_string);
    if let Some((start, _)) = exact_matches.next() {
        if exact_matches.next().is_some() {
            let match_count = 2 + exact_matches.count();
            return Err(format!(
                "old_text matches {match_count} times; must match exactly once"
            ));
        }
        return Ok(MatchOutcome {
            start,
            end: start + old_string.len(),
            used_whitespace_flex: false,
        });
    }

    // 2. Whitespace-flexible fallback
    try_flexible_line_match(content, old_string)
}

#[async_trait]
impl Tool for FileEditTool {
    fn spec(&self) -> ToolSpec { Self::spec_value() }

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("").trim_start_matches('/');
        let old_text = params["old_text"].as_str().unwrap_or("");
        let new_text = params["new_text"].as_str().unwrap_or("");

        if path.is_empty() || old_text.is_empty() {
            return Ok(ToolResult { ok: false, content: String::new(), error: Some("Missing required parameters".to_string()), token_usage: None });
        }

        let full_path = Path::new(&self.work_dir).join(path);
        tracing::debug!(
            work_dir = %self.work_dir,
            input_path = %path,
            full_path = %full_path.display(),
            exists = full_path.exists(),
            "file_edit: resolving path"
        );

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    work_dir = %self.work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_edit: failed to read file"
                );
                return Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to read file: {e}")), token_usage: None })
            }
        };

        // Resolve the match — exact first, then whitespace-flexible fallback
        let match_outcome = match resolve_match(&content, old_text) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(error),
                    token_usage: None,
                });
            }
        };

        if match_outcome.end < match_outcome.start || match_outcome.end > content.len() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Internal matching error: invalid replacement range".into()),
                token_usage: None,
            });
        }

        let mut new_content = String::with_capacity(
            content.len() - (match_outcome.end - match_outcome.start) + new_text.len(),
        );
        new_content.push_str(&content[..match_outcome.start]);
        new_content.push_str(new_text);
        new_content.push_str(&content[match_outcome.end..]);

        match tokio::fs::write(&full_path, &new_content).await {
            Ok(()) => Ok(ToolResult {
                ok: true,
                content: format!(
                    "Edited {path}: replaced 1 occurrence ({} bytes){}",
                    new_content.len(),
                    if match_outcome.used_whitespace_flex {
                        " (matched with whitespace flexibility)"
                    } else {
                        ""
                    }
                ),
                error: None,
                token_usage: None,
            }),
            Err(e) => {
                tracing::warn!(
                    work_dir = %self.work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_edit: failed to write file"
                );
                Ok(ToolResult { ok: false, content: String::new(), error: Some(format!("Failed to write file: {e}")), token_usage: None })
            }
        }
    }
}
