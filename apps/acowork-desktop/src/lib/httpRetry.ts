/**
 * httpRetry — shared 503 retry helpers
 *
 * Single source of truth for the "Gateway not ready yet" retry loop
 * used by every store-level fetcher (workspaces, file tree, …).
 * Honours the Gateway's `Retry-After` header (RFC 7231 §7.1.3) and
 * caps wall-clock budget so a stuck Gateway cannot wedge the UI.
 *
 * Bug B v3 fix — §4.13 of `docs/zh/protocols/http.md`.
 *
 * Design:
 *   - `parseRetryAfterMs` is a pure function (no I/O), so it can be
 *     unit-tested directly.
 *   - `with503Retry` wraps a fetcher that returns a `Response`. The
 *     helper is `Response`-agnostic so it works equally well for
 *     workspace lists (JSON) and tree entries (JSON) — every caller
 *     decides what to do with the final response.
 *   - AbortSignal is threaded through so workspace switches / agent
 *     switches / explicit user cancellation tear down the retry loop.
 */

export interface Retry503Policy {
  /** Maximum retry attempts (count, not wall-clock). */
  maxRetries: number;
  /** Exponential backoff base, ms. Doubles per attempt until cap. */
  backoffBaseMs: number;
  /** Maximum backoff between retries, ms. */
  backoffCapMs: number;
  /** Hard wall-clock budget — give up if exceeded even with retries left. */
  totalBudgetMs: number;
}

export const DEFAULT_503_RETRY: Retry503Policy = {
  maxRetries: 12,
  backoffBaseMs: 1000,
  backoffCapMs: 15_000,
  totalBudgetMs: 60_000,
};

/**
 * Minimal logger shape — every store already exports a `log` module
 * with debug/warn/error. We duck-type so this util stays logger-free
 * and tree-shakable.
 */
export interface RetryLogger {
  debug?: (...args: unknown[]) => void;
  warn?: (...args: unknown[]) => void;
  error?: (...args: unknown[]) => void;
}

/**
 * Parse a `Retry-After` header value (RFC 7231 §7.1.3) to milliseconds.
 * Accepts either:
 *   - delta-seconds form (e.g. `"5"`) — the form the Gateway emits
 *   - HTTP-date form (`"Wed, 21 Oct 2015 07:28:00 GMT"`) — fallback
 *
 * Returns:
 *   - `number >= 0` — milliseconds until retry
 *   - `-1`            — sentinel for Gateway SHUTTING_DOWN (caller aborts)
 *   - `null`          — header absent or unparseable (caller falls back to backoff)
 */
export function parseRetryAfterMs(headerValue: string | null): number | null {
  if (!headerValue) return null;
  const trimmed = headerValue.trim();
  // `Number("")` is 0 — an all-whitespace value must be "no hint",
  // not "retry immediately".
  if (trimmed === "") return null;
  // delta-seconds
  const seconds = Number(trimmed);
  if (Number.isFinite(seconds)) {
    // Retry-After: -1 is the Gateway's SHUTTING_DOWN sentinel. Return
    // it verbatim (NOT scaled to ms) so with503Retry's `=== -1` check
    // fires — `-1 * 1000` would silently turn the sentinel into an
    // ordinary 1s retry hint and the abort path would never trigger.
    if (seconds === -1) return -1;
    return Math.round(seconds * 1000);
  }
  // HTTP-date — RFC 7231 §7.1.3 date strings always contain letters,
  // delta-seconds never do. Gate on that so V8's lenient Date.parse
  // cannot turn numeric garbage like "1.5.3" into a plausible date.
  if (!/[A-Za-z]/.test(trimmed)) return null;
  const t = Date.parse(trimmed);
  if (Number.isFinite(t)) return Math.max(0, t - Date.now());
  return null;
}

export interface With503RetryOptions {
  policy?: Retry503Policy;
  logger?: RetryLogger;
  /** Tag prepended to log lines (e.g. "WorkspaceStore.fetchWorkspaces"). */
  tag?: string;
  /** AbortSignal — when fired, throws DOMException("AbortError"). */
  signal?: AbortSignal;
  /**
   * Current "attempt" index when wrapping a fetcher that already has
   * its own retry counter (e.g. tests). Defaults to 0.
   */
  initialAttempt?: number;
}

/**
 * Wrap a fetcher with 503 retry semantics. Invokes `fetcher` until
 * either:
 *   - it returns a non-503 Response — that Response is returned
 *   - Retry-After: -1 (SHUTTING_DOWN) — last Response is returned
 *   - `maxRetries` exceeded — last Response is returned
 *   - wall-clock budget exceeded — last Response is returned
 *   - `signal` aborts — throws DOMException("AbortError")
 *   - `fetcher` throws — error propagates
 *
 * The returned Response is the LATEST one — callers MUST handle the
 * possibility that it is still a 503 if all retries were exhausted
 * (otherwise we'd silently swallow a real failure).
 */
export async function with503Retry(
  fetcher: (signal?: AbortSignal) => Promise<Response>,
  opts: With503RetryOptions = {},
): Promise<Response> {
  const policy = opts.policy ?? DEFAULT_503_RETRY;
  const log = opts.logger;
  const tag = opts.tag ?? "httpRetry";
  const startedAt = Date.now();
  let attempt = opts.initialAttempt ?? 0;
  // eslint-disable-next-line no-constant-condition
  while (true) {
    if (opts.signal?.aborted) {
      throw new DOMException("Aborted", "AbortError");
    }
    const resp = await fetcher(opts.signal);
    if (resp.status !== 503) return resp;

    // 503 — decide what to do next
    if (Date.now() - startedAt > policy.totalBudgetMs) {
      log?.warn?.(`[${tag}] exceeded retry budget (${policy.totalBudgetMs}ms); giving up`);
      return resp;
    }
    const retryAfterMs = parseRetryAfterMs(resp.headers.get("retry-after"));
    if (retryAfterMs === -1) {
      log?.error?.(`[${tag}] aborted: Gateway SHUTTING_DOWN`);
      return resp;
    }
    if (attempt >= policy.maxRetries) {
      log?.warn?.(`[${tag}] exhausted ${policy.maxRetries} retries; giving up`);
      return resp;
    }
    const backoff = retryAfterMs != null && retryAfterMs >= 0
      ? retryAfterMs
      : Math.min(
          policy.backoffBaseMs * Math.pow(2, attempt),
          policy.backoffCapMs,
        );
    log?.debug?.(`[${tag}] got 503, retrying in ${backoff}ms (attempt=${attempt})`);
    attempt += 1;
    await new Promise((resolve) => setTimeout(resolve, backoff));
  }
}