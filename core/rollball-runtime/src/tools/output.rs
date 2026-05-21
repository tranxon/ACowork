//! Shared output-size safety helpers for built-in tools.
//!
//! Every tool that can produce unbounded output (file_read, shell,
//! content_search, etc.) must guard against the entire response being fed
//! into the LLM context window, which can exhaust the token budget and
//! crash the session task.
//!
//! Constants and helpers defined here provide consistent, project-wide
//! truncation behaviour.

/// Default maximum bytes for a single tool output (512 KB).
///
/// This is large enough to give the LLM meaningful content but small
/// enough that even with overhead (serialisation, history wrapping) the
/// result stays within typical budget margins.
pub const MAX_OUTPUT_BYTES: usize = 512 * 1024; // 512 KB

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
/// [`MAX_OUTPUT_BYTES`].
pub const TRUNCATED_OUTPUT_MARKER: &str = "\n\n[Output truncated: exceeded limit]";

/// Maximum number of results returned by collection tools (glob_search,
/// files_with_matches mode, etc.).  Prevents a tool from dumping tens of
/// thousands of paths into the output.
pub const MAX_RESULT_COUNT: usize = 1000;

/// Truncate a **single line** to [`MAX_LINE_OUTPUT_BYTES`], preserving
/// valid UTF-8 boundaries.  Returns the original string if it fits; otherwise
/// appends [`TRUNCATED_LINE_MARKER`].
pub fn truncate_line(line: &str) -> String {
    if line.len() <= MAX_LINE_OUTPUT_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_OUTPUT_BYTES;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = line[..end].to_string();
    truncated.push_str(TRUNCATED_LINE_MARKER);
    truncated
}

/// Truncate a **full output string** to [`MAX_OUTPUT_BYTES`], preserving
/// valid UTF-8 boundaries.  Appends [`TRUNCATED_OUTPUT_MARKER`] when
/// truncation occurs.
///
/// Returns `(maybe_truncated_string, was_truncated_bool)`.
pub fn truncate_output(output: &str) -> (String, bool) {
    if output.len() <= MAX_OUTPUT_BYTES {
        return (output.to_string(), false);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = output[..end].to_string();
    truncated.push_str(TRUNCATED_OUTPUT_MARKER);
    (truncated, true)
}

/// Truncate a string to `max_bytes` while preserving UTF-8 character
/// boundaries.  Returns a sub-slice (no allocation).
pub fn truncate_utf8(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}
