//! Small shared utilities for the Gateway.
//!
//! Kept deliberately minimal — only helpers used across two or more
//! modules live here; anything with a single consumer stays in its
//! owning module.

/// Diagnostic helper: produce a `first_4...last_4` preview of a
/// sensitive value (API keys, secrets) for correlating byte ranges
/// across log lines without ever leaking the full value.
///
/// Short values return `<N>` (length only). The preview NEVER exposes
/// more than the first and last 4 bytes of the input. Mirrors the
/// runtime-side preview (`build_provider_for` in
/// `acowork-runtime/src/agent/session_core.rs`) so the post-store,
/// post-decrypt, post-publish, and runtime-seen byte ranges can be
/// matched by eye (or by `grep` on the log files).
pub(crate) fn preview_key(k: &str) -> String {
    let len = k.len();
    if len <= 8 {
        format!("<{}>", len)
    } else {
        format!("{}...{}", &k[..4], &k[len - 4..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preview_key() {
        assert_eq!(preview_key(""), "<0>");
        assert_eq!(preview_key("short"), "<5>");
        assert_eq!(preview_key("sk-c1234567890abcdefUiAI"), "sk-c...UiAI");
    }
}
