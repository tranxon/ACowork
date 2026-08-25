//! Document reader sub-modules — format-specific text extraction.
//!
//! Each module handles one document format and exposes a single
//! `extract_text(path, options) -> Result<String>` function.
//!
//! | Format    | Crate        | Strategy                          |
//! |-----------|-------------|-----------------------------------|
//! | PDF       | `pdf-extract` | Font-rendered text extraction    |
//! | DOCX      | `zip`+`quick-xml` | XML text extraction          |
//! | PPTX      | `zip`+`quick-xml` | Slide text extraction        |
//! | XLSX      | `calamine`   | Sheet / row iteration             |
//! | Text      | (std)        | Strict UTF-8 read with NUL sniff  |
//!
//! `text` is the fallback for everything else: Markdown, source code,
//! logs, config files — any UTF-8 file. See [`text`] for the safety
//! gates (size cap, NUL sniff, strict decoding).

pub mod docx;
pub mod pdf;
pub mod pptx;
pub mod text;
pub mod xlsx;

use std::path::Path;

/// Options controlling text extraction behaviour.
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Optional start page (1-based, PDF / DOCX / PPTX).
    pub start_page: Option<usize>,
    /// Optional end page (inclusive, PDF / DOCX / PPTX).
    pub end_page: Option<usize>,
    /// Whether to render tables as Markdown (DOCX / XLSX).
    pub include_tables: bool,
}

/// Detect the document format from a file extension.
pub fn detect_format(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => Some("pdf"),
        Some("docx") => Some("docx"),
        Some("pptx") => Some("pptx"),
        Some("xlsx") => Some("xlsx"),
        _ => None,
    }
}

// ── Tool trait implementation ───────────────────────────────────────────

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::output;

/// Maximum file size for document reading (50 MB).
const MAX_DOC_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Built-in document reader tool.
///
/// Reads PDF, DOCX, PPTX, and XLSX files and extracts their text content.
/// Falls back to UTF-8 plain-text extraction for every other file type
/// (Markdown, source code, logs, ...).
pub struct DocReaderTool;

impl Default for DocReaderTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DocReaderTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "doc_reader".to_string(),
            description: "Read and extract text from documents (PDF, DOCX, PPTX, XLSX) and \
                 UTF-8 plain-text files (Markdown, source code, logs, ...). \
                 Use this tool to ingest file content for analysis. \
                 Accepts both relative paths (within workspace) and absolute paths. \
                 Returns plain text with structural markers (page/slide/sheet headers, \
                 optional Markdown tables)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the document file (relative or absolute)"
                    },
                    "start_page": {
                        "type": "integer",
                        "description": "Optional 1-based start page/slide/sheet (default: 1)"
                    },
                    "end_page": {
                        "type": "integer",
                        "description": "Optional inclusive end page/slide/sheet"
                    },
                    "include_tables": {
                        "type": "boolean",
                        "description": "Render tables as Markdown (DOCX/XLSX, default: false)"
                    }
                },
                "required": ["path"]
            }),
        }
    }
}

#[async_trait]
impl Tool for DocReaderTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let raw_path = params["path"].as_str().unwrap_or("");
        if raw_path.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
                token_usage: None,
            });
        }

        // Support both relative and absolute paths. Absolute paths (e.g. from
        // Gateway document upload) bypass work_dir join — see
        // `acowork_core::path_utils::resolve` for the full rule set.
        let full_path = acowork_core::path_utils::resolve(raw_path, work_dir);

        // Size check
        match tokio::fs::metadata(&full_path).await {
            Ok(meta) => {
                if meta.len() > MAX_DOC_SIZE_BYTES {
                    return Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "Document too large: {} bytes (limit: {MAX_DOC_SIZE_BYTES} bytes)",
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

        // Detect format. PDF/DOCX/PPTX/XLSX get their dedicated
        // extractor; anything else falls back to UTF-8 plain-text
        // extraction — this is what makes Markdown / source code / log
        // uploads readable (the blob store collapses unknown extensions
        // to `.bin`, and `safe_extension` never invents new suffixes).
        let format = match detect_format(&full_path) {
            Some(f) => f,
            None => {
                let full_path_clone = full_path.clone();
                let raw = tokio::task::spawn_blocking(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        text::extract_text(&full_path_clone)
                    }))
                    .map_err(|panic_payload| {
                        let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                            s.clone()
                        } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                            s.to_string()
                        } else {
                            "Unknown panic during text extraction".to_string()
                        };
                        format!("Text extraction panicked: {msg}")
                    })
                    .and_then(|r| r)
                })
                .await
                .map_err(|join_err| {
                    format!("Text extraction task cancelled or panicked: {join_err}")
                })
                .and_then(|r| r);

                return match raw {
                    Ok(text) => {
                        let (truncated, was_truncated) = output::truncate_output(&text);
                        Ok(ToolResult {
                            ok: true,
                            content: truncated,
                            error: if was_truncated {
                                Some(
                                    "Output truncated: text content exceeded the maximum output size."
                                        .to_string(),
                                )
                            } else {
                                None
                            },
                            token_usage: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "Unsupported document format: '{}'. Supported: pdf, docx, pptx, xlsx, \
                             or UTF-8 plain text (md, txt, source code, ...). {e}",
                            full_path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("(none)")
                        )),
                        token_usage: None,
                    }),
                };
            }
        };

        // Build extract options from params
        let opts = ExtractOptions {
            start_page: params["start_page"].as_u64().map(|v| v as usize),
            end_page: params["end_page"].as_u64().map(|v| v as usize),
            include_tables: params["include_tables"].as_bool().unwrap_or(false),
        };

        // Dispatch to format-specific extraction on a blocking thread.
        //
        // PDF/DOCX/PPTX/XLSX extraction is inherently CPU-bound and may
        // block for seconds on large or complex documents (e.g. PDFs with
        // embedded fonts / tables).  Running this on a tokio worker thread
        // would starve other async tasks and, worse, a panic inside the
        // extraction crate (e.g. pdf_extract font rendering) would kill
        // the owning tokio task (SessionTask).
        //
        // spawn_blocking isolates the heavy work on a dedicated thread pool
        // and catch_unwind converts any internal panic into a clean error.
        let full_path_clone = full_path.clone();
        let opts_clone = opts.clone();
        let raw = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
                "pdf" => pdf::extract_text(&full_path_clone, &opts_clone),
                "docx" => docx::extract_text(&full_path_clone, &opts_clone),
                "pptx" => pptx::extract_text(&full_path_clone, &opts_clone),
                "xlsx" => xlsx::extract_text(&full_path_clone, &opts_clone),
                _ => unreachable!(),
            }))
            .map_err(|panic_payload| {
                let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "Unknown panic during document extraction".to_string()
                };
                format!("Document extraction panicked: {msg}")
            })
            .and_then(|r| r)
        })
        .await
        .map_err(|join_err| format!("Document extraction task cancelled or panicked: {join_err}"))
        .and_then(|r| r);

        match raw {
            Ok(text) => {
                let (truncated, was_truncated) = output::truncate_output(&text);
                Ok(ToolResult {
                    ok: true,
                    content: truncated,
                    error: if was_truncated {
                        Some(
                            "Output truncated: document content exceeded the maximum output size."
                                .to_string(),
                        )
                    } else {
                        None
                    },
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(e),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a temp file and return `(dir, path)`. Keeping `dir` alive
    /// for the rest of the test guarantees the file exists and that the
    /// directory is cleaned up on drop.
    fn with_temp_file(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(bytes).expect("write temp file");
        (dir, path)
    }

    #[test]
    fn detect_format_returns_none_for_text_extensions() {
        for name in ["notes.md", "main.rs", "Makefile", "app.json", "blob.bin"] {
            assert_eq!(detect_format(Path::new(name)), None, "{name}");
        }
    }

    #[test]
    fn detect_format_still_resolves_documents() {
        for name in ["a.pdf", "b.docx", "c.pptx", "d.xlsx"] {
            assert!(detect_format(Path::new(name)).is_some(), "{name}");
        }
    }

    #[tokio::test]
    async fn execute_reads_text_file_without_extension_whitelist() {
        // End-to-end fallback: a `.md` path (which after an upload lands
        // as `.bin` on disk) must be readable as plain text — this is
        // the contract that makes arbitrary text uploads usable.
        let (_dir, p) = with_temp_file(b"# Hello\n", "notes.md");
        let result = DocReaderTool::new()
            .execute(serde_json::json!({ "path": p.to_string_lossy() }), None)
            .await
            .expect("execute should not fail");
        assert!(result.ok, "text fallback must succeed: {:?}", result.error);
        assert_eq!(result.content, "# Hello\n");
    }

    #[tokio::test]
    async fn execute_rejects_binary_as_unreadable() {
        let (_dir, p) = with_temp_file(b"PK\x03\x04\x00\x00", "fake.zip");
        let result = DocReaderTool::new()
            .execute(serde_json::json!({ "path": p.to_string_lossy() }), None)
            .await
            .expect("execute should not fail");
        assert!(!result.ok);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("Unsupported document format"), "got: {err}");
    }
}
