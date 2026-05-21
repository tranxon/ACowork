//! Document reader sub-modules — format-specific text extraction.
//!
//! Each module handles one document format and exposes a single
//! `extract_text(path, options) -> Result<String>` function.
//!
//! | Format | Crate        | Strategy               |
//! |--------|-------------|------------------------|
//! | PDF    | `lopdf`      | Object stream decoding |
//! | DOCX   | `zip`+`quick-xml` | XML text extraction |
//! | PPTX   | `zip`+`quick-xml` | Slide text extraction |
//! | XLSX   | `calamine`   | Sheet / row iteration  |

pub mod pdf;
pub mod docx;
pub mod pptx;
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

use async_trait::async_trait;
use rollball_core::tools::traits::{Tool, ToolResult, ToolSpec};
use serde_json::Value;

use crate::tools::output;

/// Maximum file size for document reading (50 MB).
const MAX_DOC_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Built-in document reader tool.
///
/// Reads PDF, DOCX, PPTX, and XLSX files and extracts their text content.
pub struct DocReaderTool {
    work_dir: String,
}

impl DocReaderTool {
    pub fn new(work_dir: &str) -> Self {
        Self { work_dir: work_dir.to_string() }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "doc_reader".to_string(),
            description:
                "Read and extract text from documents (PDF, DOCX, PPTX, XLSX). \
                 Use this tool to ingest document content for analysis. \
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

    async fn execute(&self, params: Value) -> rollball_core::error::Result<ToolResult> {
        let raw_path = params["path"].as_str().unwrap_or("");
        if raw_path.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
                token_usage: None,
            });
        }

        // Support both relative and absolute paths.
        // Absolute paths (e.g. from Gateway document upload) bypass work_dir join.
        let is_absolute = raw_path.starts_with('/') || (raw_path.len() > 2 && raw_path.as_bytes()[1] == b':');
        let full_path = if is_absolute {
            std::path::PathBuf::from(raw_path)
        } else {
            let relative = raw_path.trim_start_matches('/');
            std::path::Path::new(&self.work_dir).join(relative)
        };

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

        // Detect format
        let format = match detect_format(&full_path) {
            Some(f) => f,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Unsupported document format: '{}'. Supported: pdf, docx, pptx, xlsx",
                        full_path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("(none)")
                    )),
                    token_usage: None,
                });
            }
        };

        // Build extract options from params
        let opts = ExtractOptions {
            start_page: params["start_page"].as_u64().map(|v| v as usize),
            end_page: params["end_page"].as_u64().map(|v| v as usize),
            include_tables: params["include_tables"].as_bool().unwrap_or(false),
        };

        // Dispatch to format-specific extraction
        let raw = match format {
            "pdf" => pdf::extract_text(&full_path, &opts),
            "docx" => docx::extract_text(&full_path, &opts),
            "pptx" => pptx::extract_text(&full_path, &opts),
            "xlsx" => xlsx::extract_text(&full_path, &opts),
            _ => unreachable!(),
        };

        match raw {
            Ok(text) => {
                let (truncated, was_truncated) = output::truncate_output(&text);
                Ok(ToolResult {
                    ok: true,
                    content: truncated,
                    error: if was_truncated {
                        Some("Output truncated: document content exceeded the maximum output size."
                            .to_string())
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
