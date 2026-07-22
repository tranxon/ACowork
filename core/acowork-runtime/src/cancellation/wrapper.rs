//! Future helpers for combining [`CancellationToken`] with other futures.
//!
//! The primary export is [`select_cancelled`], which races an arbitrary
//! `Result`-producing future against a cancellation token. When the token
//! wins, the inner future is **dropped**, which performs cleanup of any
//! resources it holds (e.g. an in-flight `reqwest::send().await` is aborted).
//!
//! This is the key primitive that fixes the LLM TTFT stop bug (ADR §1.3,
//! §4.4) without requiring a Provider trait refactor.

use std::future::Future;

use crate::error::Result;

use super::token::CancellationToken;

/// Race a future against a cancellation token.
///
/// Returns:
/// - `Ok(Some(value))` — the inner future completed first; `value` is its result.
/// - `Ok(None)` — the token was cancelled; the inner future was dropped.
/// - `Err(e)` — the inner future failed before cancellation.
///
/// # Drop semantics (key design point — see ADR §4.4)
///
/// When the cancellation branch wins, `fut` is dropped at the `select!` await
/// point. For futures holding resources, this performs automatic cleanup:
/// dropping an in-flight `reqwest::send().await` aborts the HTTP request and
/// releases the connection. The runtime therefore returns control to the user
/// within milliseconds of `cancel()` being called, regardless of how deep the
/// inner future is in its setup (TCP connect, TLS handshake, waiting for SSE
/// headers, etc.).
///
/// # Trade-off (documented in ADR §7.1 R1)
///
/// After cancellation, the original task may continue executing in the
/// background for a short period until its I/O completes or times out at the
/// OS level. This is acceptable: the user perceives an immediate stop, and the
/// background task self-cleans. A future improvement may expose an explicit
/// abort handle to forcibly terminate the task, but that requires a Provider
/// trait refactor (out of scope per ADR §8).
///
/// # Cancel race
///
/// If `cancel()` is called *before* `select_cancelled` is awaited, the
/// `biased;` ordering ensures the cancel branch wins on first poll — the
/// inner future is never spawned.
pub async fn select_cancelled<T, F>(token: CancellationToken, fut: F) -> Result<Option<T>>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        // biased: prefer the cancel branch on first poll. This makes the
        // "cancel-before-await" case resolve immediately rather than racing
        // against an already-cancellable inner future.
        biased;
        _ = token.cancelled() => Ok(None),
        result = fut => result.map(Some),
    }
}