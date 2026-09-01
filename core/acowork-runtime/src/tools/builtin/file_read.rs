//! File read tool — reads a line range (fragment) from a file within the workspace

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::output;

const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB

/// File read tool — fragment reader, not a whole-file reader
pub struct FileReadTool;

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_read".to_string(),
            description: "Read a specific range of lines from a file, with line numbers. This is a fragment reader — both start_line (1-based) and end_line (inclusive) are required. Read at most 400 lines per call; for longer ranges, paginate across multiple calls. Always use content_search first to locate the relevant line numbers before calling this tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" },
                    "start_line": { "type": "integer", "description": "Starting line number (1-based). Required. Must be > 0." },
                    "end_line": { "type": "integer", "description": "Ending line number (inclusive). Required. Must be >= start_line. At most 400 lines per call — paginate if you need more." }
                },
                "required": ["path", "start_line", "end_line"]
            }),
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("");
        if path.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
                token_usage: None,
            });
        }

        // start_line and end_line are required
        let start_line_raw = match params["start_line"].as_u64() {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required 'start_line' parameter. Use content_search first to locate line numbers, then request a specific range (≤100 lines).".to_string()),
                    token_usage: None,
                });
            }
        };
        let end_line_raw = match params["end_line"].as_u64() {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required 'end_line' parameter".to_string()),
                    token_usage: None,
                });
            }
        };

        // Validate range
        if start_line_raw == 0 {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("start_line must be >= 1 (1-based)".to_string()),
                token_usage: None,
            });
        }
        if end_line_raw < start_line_raw {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "end_line ({end_line_raw}) must be >= start_line ({start_line_raw})"
                )),
                token_usage: None,
            });
        }
        let requested = (end_line_raw - start_line_raw + 1) as usize;
        if requested > super::MAX_LINES_PER_CALL {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "Range too large: {requested} lines requested, max {max} per call. Paginate across multiple calls (e.g. start_line: {s}, end_line: {e1}, then start_line: {e1p1}, end_line: {e2}...)",
                    max = super::MAX_LINES_PER_CALL,
                    s = start_line_raw,
                    e1 = start_line_raw + super::MAX_LINES_PER_CALL as u64 - 1,
                    e1p1 = start_line_raw + super::MAX_LINES_PER_CALL as u64,
                    e2 = end_line_raw,
                )),
                token_usage: None,
            });
        }

        let full_path = acowork_core::path_utils::resolve(path, work_dir);
        tracing::debug!(
            work_dir = ?work_dir,
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
                    return Ok(ToolResult {
                        ok: true,
                        content: "[File is empty]".to_string(),
                        error: None,
                        token_usage: None,
                    });
                }

                let s = (start_line_raw as usize).saturating_sub(1).min(total);
                let e = (end_line_raw as usize).min(total);

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

                let summary = format!("\n[Lines {}-{} of {total}]", s + 1, e);

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
                    work_dir = ?work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_read: failed to read file"
                );
                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                    token_usage: None,
                })
            }
        }
    }
}

// ── E2E ─────────────────────────────────────────────────────────
//
// file_read previously had zero integration coverage. These tests go
// through `Tool::execute()` (the LLM-facing entry point) and verify
// every error path the LLM could hit, plus the happy path.

#[cfg(test)]
mod tests {
    use super::super::MAX_LINES_PER_CALL;
    use super::*;
    use std::io::Write;

    fn with_temp_file(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(bytes).expect("write temp file");
        (dir, path)
    }

    fn build_lines_file(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut buf = Vec::new();
        for i in 1..=n {
            buf.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        with_temp_file(&buf, "lines.txt")
    }

    // ── Happy path ─────────────────────────────────────────────

    #[tokio::test]
    async fn e2e_reads_middle_range_of_100_line_file() {
        let (_dir, p) = build_lines_file(100);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 10,
                    "end_line": 20
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok, "happy path must succeed: {:?}", result.error);
        // Numbered output: line 10 starts at column 1, etc.
        assert!(
            result.content.contains("10: line 10"),
            "got: {}",
            result.content
        );
        assert!(result.content.contains("20: line 20"));
        // Lines outside the requested range must NOT appear
        assert!(!result.content.contains("9: line 9"));
        assert!(!result.content.contains("21: line 21"));
        // Summary marker
        assert!(
            result.content.contains("[Lines 10-20 of 100]"),
            "got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn e2e_single_line_range() {
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 5,
                    "end_line": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("5: line 5"));
        assert!(!result.content.contains("4: line 4"));
        assert!(!result.content.contains("6: line 6"));
        assert!(result.content.contains("[Lines 5-5 of 10]"));
    }

    // ── Required-parameter errors ──────────────────────────────

    #[tokio::test]
    async fn e2e_missing_start_line_errors() {
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "end_line": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("start_line"),
            "error must name the missing field; got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn e2e_missing_end_line_errors() {
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("end_line"),
            "error must name the missing field"
        );
    }

    #[tokio::test]
    async fn e2e_missing_path_errors() {
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "start_line": 1,
                    "end_line": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.as_deref().unwrap().contains("path"));
    }

    // ── Parameter validation ────────────────────────────────────

    #[tokio::test]
    async fn e2e_start_line_zero_errors_as_one_based() {
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 0,
                    "end_line": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.as_deref().unwrap().contains("1-based"));
    }

    #[tokio::test]
    async fn e2e_end_less_than_start_errors() {
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 5,
                    "end_line": 3
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("end_line"), "got: {err}");
        assert!(err.contains("start_line"), "got: {err}");
    }

    #[tokio::test]
    async fn e2e_range_over_max_lines_per_call_errors_with_paginate_template() {
        // MAX_LINES_PER_CALL is 400. Request 500 → must error AND
        // include the explicit paginate template so the LLM can fix
        // its call without trial-and-error.
        let (_dir, p) = build_lines_file(1000);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 500
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("Range too large"), "got: {err}");
        assert!(
            err.contains("400"),
            "must mention MAX_LINES_PER_CALL: {err}"
        );
        assert!(
            err.contains("Paginate"),
            "must offer paginate template: {err}"
        );
    }

    #[tokio::test]
    async fn e2e_range_exactly_at_max_lines_succeeds() {
        // Boundary: exactly MAX_LINES_PER_CALL lines must work.
        let (_dir, p) = build_lines_file(MAX_LINES_PER_CALL);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": MAX_LINES_PER_CALL
                }),
                None,
            )
            .await
            .unwrap();
        assert!(
            result.ok,
            "exact MAX_LINES_PER_CALL must succeed: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn e2e_range_one_over_max_lines_errors() {
        let (_dir, p) = build_lines_file(MAX_LINES_PER_CALL + 1);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": MAX_LINES_PER_CALL + 1
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.as_deref().unwrap().contains("Range too large"));
    }

    // ── Out-of-range handling ───────────────────────────────────

    #[tokio::test]
    async fn e2e_start_beyond_file_length_returns_informative_marker() {
        // file_read's contract: start > total → returns ok=true with
        // a "[No lines in range, file has N lines]" marker. The LLM
        // can recover by re-issuing with a smaller range.
        let (_dir, p) = build_lines_file(5);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 100,
                    "end_line": 105
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("No lines in range"),
            "got: {}",
            result.content
        );
        assert!(
            result.content.contains("5 lines"),
            "must report actual line count: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn e2e_end_clamped_to_file_length() {
        // file_read clamps `end` to total silently. The LLM gets
        // back everything from start_line to EOF.
        //
        // Important: end must stay under MAX_LINES_PER_CALL (400),
        // otherwise the range-size check rejects before the clamp can
        // happen. We pick end=15 so the range size (15-8+1 = 8 lines)
        // is well under the cap, while end (15) is still beyond
        // the file length (10) — this is the actual clamp path we
        // want to exercise.
        let (_dir, p) = build_lines_file(10);
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 8,
                    "end_line": 15
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok, "result.ok: {:?}", result.error);
        assert!(result.content.contains("8: line 8"));
        assert!(result.content.contains("10: line 10"));
        assert!(result.content.contains("[Lines 8-10 of 10]"));
        // And no line beyond the file's end should leak.
        assert!(!result.content.contains("11: "));
    }

    // ── I/O errors ──────────────────────────────────────────────

    #[tokio::test]
    async fn e2e_nonexistent_file_errors_with_clear_message() {
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": "/this/path/should/never/exist/in/the/test/xyz123.txt",
                    "start_line": 1,
                    "end_line": 5
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("Failed to read file"), "got: {err}");
    }

    #[tokio::test]
    async fn e2e_empty_file_returns_special_marker() {
        let (_dir, p) = with_temp_file(b"", "empty.txt");
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 1
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(result.content, "[File is empty]");
    }

    // ── UTF-8 content ───────────────────────────────────────────

    #[tokio::test]
    async fn e2e_utf8_cjk_content_preserved_byte_intact() {
        // CJK is the testbed for UTF-8 safety — each character is 3
        // bytes and file_read must not corrupt mid-character boundaries.
        let lines = ["中文第一行", "中文第二行", "中文第三行", "中文第四行"];
        let mut buf = Vec::new();
        for l in &lines {
            buf.extend_from_slice(l.as_bytes());
            buf.push(b'\n');
        }
        let (_dir, p) = with_temp_file(&buf, "cjk.txt");
        let result = FileReadTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 2,
                    "end_line": 3
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.content.contains("中文第二行"),
            "got: {}",
            result.content
        );
        assert!(result.content.contains("中文第三行"));
        assert!(!result.content.contains("中文第一行"));
        assert!(!result.content.contains("中文第四行"));
    }

    // ── Spec exposure ───────────────────────────────────────────

    #[test]
    fn e2e_spec_requires_start_line_and_end_line() {
        // LLM sees this schema. If start_line / end_line are NOT
        // listed as required, every model will forget to pass them.
        let spec = FileReadTool::spec_value();
        let required: Vec<&str> = spec.input_schema["required"]
            .as_array()
            .expect("required array present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"path"), "required: {required:?}");
        assert!(required.contains(&"start_line"), "required: {required:?}");
        assert!(required.contains(&"end_line"), "required: {required:?}");
    }

    #[test]
    fn e2e_spec_description_advertises_max_lines_per_call() {
        // The spec description is the only place the LLM learns about
        // the 400-line cap. If this drifts, models will request huge
        // ranges and waste a round-trip on the error.
        let desc = FileReadTool::spec_value().description;
        assert!(
            desc.contains("400"),
            "must mention the 400-line cap: {desc}"
        );
        assert!(desc.contains("paginate"), "must teach pagination: {desc}");
    }
}
