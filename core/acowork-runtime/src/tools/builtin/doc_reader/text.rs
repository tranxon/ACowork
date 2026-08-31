//! Plain-text fallback extraction for the `doc_reader` tool.
//!
//! DOCX/PPTX/XLSX/PDF have dedicated extractors; every other file type
//! (Markdown, source code, logs, config files, ...) lands here. This is
//! what makes ADR-046 uploads of arbitrary text files readable by the
//! LLM: the blob store collapses unknown extensions to `.bin` on disk,
//! and `detect_format` returns `None` for `.bin` — so without this
//! fallback the agent could upload a `.md` but never read it back.
//!
//! ## Safety gates
//!
//! Reading is strict rather than lossy:
//! - **Size cap** — Reuses `crate::tools::output::MAX_OUTPUT_BYTES`
//!   (128 KB) as a fail-fast gate *before* reading, so we don't pull
//!   megabytes off disk just to truncate them at the tool-output
//!   boundary moments later. Bigger files must be read incrementally
//!   (shell `head` / `grep`, workspace `read_file`).
//! - **NUL-byte sniff** — binary files (PNG, ZIP, EXE, ...) frequently
//!   happen to be valid UTF-8; a NUL byte is a reliable cheap signal
//!   that the file is not human-readable text.
//! - **Strict UTF-8** — `String::from_utf8` rejects invalid sequences
//!   (e.g. UTF-16, GBK, arbitrary binary) instead of silently replacing
//!   them with U+FFFD like `from_utf8_lossy` would.

use std::path::Path;

/// Maximum plain-text size the doc_reader will inline as a tool result.
///
/// Reuses `crate::tools::output::MAX_OUTPUT_BYTES` (128 KB) as a fail-fast
/// gate *before* reading, so we don't pull megabytes off disk just to
/// truncate them at the tool-output boundary moments later. Files larger
/// than this must be read incrementally (`head` / `grep` / workspace
/// `read_file`).
///
/// Kept aligned with `MAX_OUTPUT_BYTES` on purpose: one number, one rule.
/// If we ever need a different read-vs-output cap, split them again — but
/// make the divergence intentional and documented.
///
/// True if the bytes contain a NUL byte — a strong signal the file is
/// binary (or UTF-16/UTF-32 text) rather than UTF-8 text.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// Extract text from a plain-text file.
///
/// Returns `Err` with an actionable message when the file is too large,
/// binary, or not valid UTF-8.
pub fn extract_text(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read text file: {e}"))?;

    if bytes.len() > crate::tools::output::MAX_OUTPUT_BYTES {
        return Err(format!(
            "Text file too large: {} bytes (limit: {} bytes). \
             Read it incrementally instead (e.g. `head`, `grep`, or workspace `read_file`).",
            bytes.len(),
            crate::tools::output::MAX_OUTPUT_BYTES
        ));
    }

    if looks_binary(&bytes) {
        return Err(
            "File appears to be binary (contains NUL bytes); only UTF-8 text is supported."
                .to_string(),
        );
    }

    String::from_utf8(bytes).map_err(|e| {
        format!("File is not valid UTF-8 text (invalid byte at offset {}): {e}", e.utf8_error().valid_up_to())
    })
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
    fn reads_utf8_markdown() {
        let (_dir, p) = with_temp_file(b"# Title\n\nhello **world**\n", "notes.md");
        let text = extract_text(&p).expect("md should extract");
        assert!(text.contains("# Title"));
        assert!(text.contains("hello **world**"));
    }

    #[test]
    fn reads_extensionless_source_file() {
        let (_dir, p) = with_temp_file(b"fn main() {}\n", "Makefile");
        let text = extract_text(&p).expect("extensionless text should extract");
        assert_eq!(text, "fn main() {}\n");
    }

    #[test]
    fn rejects_nul_bytes_as_binary() {
        let (_dir, p) = with_temp_file(b"PNG\x00\x01\x02binary-ish", "fake.png");
        let err = extract_text(&p).expect_err("NUL bytes must be rejected");
        assert!(err.contains("binary"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_utf8() {
        // 0xFF is never valid UTF-8.
        let (_dir, p) = with_temp_file(&[0xFF, 0xFE, 0x00, 0x41], "utf16-le.txt");
        let err = extract_text(&p).expect_err("invalid UTF-8 must be rejected");
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn rejects_oversized_text() {
        let big = vec![b'a'; crate::tools::output::MAX_OUTPUT_BYTES + 1];
        let (_dir, p) = with_temp_file(&big, "big.log");
        let err = extract_text(&p).expect_err("oversized text must be rejected");
        assert!(err.contains("too large"), "got: {err}");
    }
}
