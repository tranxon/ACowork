//! Shared output-size safety helpers for built-in tools.
//!
//! Every tool that can produce unbounded output (file_read, shell,
//! content_search, etc.) must guard against the entire response being fed
//! into the LLM context window, which can exhaust the token budget and
//! crash the session task.
//!
//! Constants and helpers defined here provide consistent, project-wide
//! truncation behaviour.  The underlying character-boundary-safe slice
//! logic is the project-wide [`crate::util::text::truncate_utf8`] helper;
//! the markers and append-policies specific to *tool output* stay here.

/// Default maximum bytes for a single tool output (128 KB).
///
/// Reduced from 512 KB → 256 KB → 128 KB. Two concurrent near-cap results
/// (e.g. two bash commands) must stay within the usable context budget of
/// most models (typically 128-172K tokens): 128 KB ≈ 32K tokens each leaves
/// ~50% of the window for system prompt, tool definitions, history and
/// follow-up turns. Larger payloads would push the session task over the
/// compression threshold on a single tool call.
pub const MAX_OUTPUT_BYTES: usize = 128 * 1024; // 128 KB

/// Maximum bytes per *single matched line* when a tool emits line-level
/// output (e.g. content_search content mode).
///
/// A single line that is hundreds of KB (e.g. an inlined HTML tool_result
/// in a JSONL file) can dominate the output on its own.  Truncating
/// individual lines at 10 KB prevents this while still allowing most
/// real-world lines through unchanged.
pub const MAX_LINE_OUTPUT_BYTES: usize = 10 * 1024; // 10 KB

/// Appended when a line is truncated because it exceeded
/// [`MAX_LINE_OUTPUT_BYTES`].
pub const TRUNCATED_LINE_MARKER: &str = "...[truncated]";

/// Appended when an entire output is truncated because it exceeded
/// [`MAX_OUTPUT_BYTES`].  Provides the LLM with actionable guidance
/// so it knows to narrow the request rather than blindly retry.
pub const TRUNCATED_OUTPUT_MARKER: &str = "\
\n[OUTPUT TRUNCATED: exceeded tool-level output limit (256 KB). \
The full output was too large to fit in the LLM context window. \
SUGGESTION: re-run with more targeted parameters — narrower \
search patterns, file range limits (head/tail/Select-Object), \
or pagination across multiple calls.]";

/// Maximum number of results returned by collection tools (glob_search,
/// files_with_matches mode, etc.).  Prevents a tool from dumping tens of
/// thousands of paths into the output.
pub const MAX_RESULT_COUNT: usize = 1000;

/// Truncate a **single line** to [`MAX_LINE_OUTPUT_BYTES`], preserving
/// valid UTF-8 boundaries.  Returns the original string if it fits; otherwise
/// appends [`TRUNCATED_LINE_MARKER`].
pub fn truncate_line(line: &str) -> String {
    let truncated = crate::util::text::truncate_utf8(line, MAX_LINE_OUTPUT_BYTES);
    if truncated.len() == line.len() {
        return line.to_string();
    }
    let mut out = truncated.to_string();
    out.push_str(TRUNCATED_LINE_MARKER);
    out
}

/// Truncate a **full output string** to [`MAX_OUTPUT_BYTES`], preserving
/// valid UTF-8 boundaries.  Appends [`TRUNCATED_OUTPUT_MARKER`] when
/// truncation occurs.
///
/// Returns `(maybe_truncated_string, was_truncated_bool)`.
pub fn truncate_output(output: &str) -> (String, bool) {
    let truncated = crate::util::text::truncate_utf8(output, MAX_OUTPUT_BYTES);
    if truncated.len() == output.len() {
        return (output.to_string(), false);
    }
    let mut out = truncated.to_string();
    out.push_str(TRUNCATED_OUTPUT_MARKER);
    (out, true)
}

// Re-exported so older internal callers (none in mainline today, but
// possibly in plugins) keep working.  The single source of truth is
// [`crate::util::text::truncate_utf8`].
pub use crate::util::text::truncate_utf8;
