//! Provider module

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod registry;
pub mod reliable;
pub mod router;

/// Parse the HTTP `Retry-After` header value into milliseconds.
///
/// Supports two formats per RFC 7231:
/// - Seconds: `"120"` → `120_000` ms
/// - HTTP-date: `"Wed, 21 Oct 2015 07:28:00 GMT"` → ms until that date
///
/// Returns `None` if the header is absent, malformed, or the date is in the past.
pub fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let value_str = value.to_str().ok()?;

    // Try parsing as seconds (integer)
    if let Ok(seconds) = value_str.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    // Try parsing as HTTP-date (RFC 2822 / IMF-fixdate)
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc2822(value_str) {
        let now = chrono::Utc::now();
        let wait = datetime.signed_duration_since(now);
        let ms = wait.num_milliseconds();
        if ms > 0 {
            return Some(ms as u64);
        }
    }

    None
}
