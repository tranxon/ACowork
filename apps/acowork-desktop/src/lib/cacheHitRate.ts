/**
 * Prompt-cache hit-ratio computation (ADR-066).
 *
 * Provider billing models differ on what denominator makes sense:
 *
 *   - **Anthropic Messages** (and AWS Bedrock hosting Anthropic models)
 *     reports both `cache_read_input_tokens` and
 *     `cache_creation_input_tokens`. Writes are the cost of seeding
 *     the cache and reads are the cost of reusing it. A balanced
 *     denominator is therefore
 *     `cache_read / (input + cache_read + cache_write)`, i.e. the
 *     share of the prompt that came from cache. (Anthropic bills
 *     reads at ~10% of fresh-input price and writes at ~125% for a
 *     single 5-min write — but the hit-ratio is a UX metric, not a
 *     dollar conversion, so we just compare to total prompt cost.)
 *
 *   - **OpenAI Chat Completions** (and Azure OpenAI) reports only
 *     `prompt_tokens_details.cached_tokens`; there is no write
 *     concept (OpenAI auto-caches without a per-call write event).
 *     The natural denominator is therefore just `cache_read /
 *     prompt_tokens`.
 *
 * Other providers (`ollama`, `lmstudio`, `deepseek`, `zhipuai`,
 * `minimax*`, `volcengine-agent-plan`, …) do not surface cache
 * accounting through `UsageInfo`, so the ratio is undefined for them
 * and `computeCacheHitRate` returns `null`.
 *
 * The choice of denominator is deliberately a **front-end UX
 * decision**: the Runtime just forwards raw provider counts and lets
 * the UI render them with whatever semantics the designer prefers.
 * See ADR-066 §6.
 */

/**
 * Cache-accounting protocol family a provider belongs to.
 *
 * - `"openai"`     — OpenAI Chat Completions (cached via
 *                    `prompt_tokens_details.cached_tokens`).
 * - `"anthropic"`  — Anthropic Messages, plus AWS Bedrock when
 *                    hosting an Anthropic model (same response
 *                    shape).
 *
 * Returns `null` for providers that don't surface prompt-cache
 * accounting (local models, providers without a write event, etc.).
 */
export type CacheProtocol = "openai" | "anthropic";

/**
 * Resolve which cache protocol family a provider id belongs to.
 *
 * The lookup is intentionally **explicit** (not a fuzzy prefix
 * match): an unrecognised provider id returns `null` rather than
 * guessing, because mis-classifying would either (a) under-report
 * the hit rate (using a larger denominator than the provider bills
 * against) or (b) display misleading percentages for providers that
 * don't actually report cache tokens. Both outcomes are worse than
 * simply not showing the ratio.
 *
 * To add support for a new cache-aware provider, extend the match
 * below — and add a vitest case to `cacheHitRate.test.ts`.
 */
export function getCacheProtocol(
  providerId: string | null | undefined,
): CacheProtocol | null {
  if (!providerId) return null;
  // OpenAI Chat Completions protocol family.
  //   `openai`            — direct OpenAI / Azure OpenAI
  //   `azure`             — shorthand used by self-hosted configs
  //   `azure-openai`      — verbose Azure form
  if (
    providerId === "openai" ||
    providerId === "azure" ||
    providerId === "azure-openai"
  ) {
    return "openai";
  }
  // Anthropic Messages protocol family.
  //   `anthropic`  — Anthropic direct
  //   `bedrock`    — AWS Bedrock (only Anthropic models surface
  //                  cache_read_input_tokens; non-Anthropic Bedrock
  //                  models will keep cache counters at 0, which the
  //                  denominator-zero guard below handles correctly)
  if (providerId === "anthropic" || providerId === "bedrock") {
    return "anthropic";
  }
  return null;
}

export interface CacheUsageLike {
  /** Per-turn cache-read tokens (last LLM call only). */
  cache_read_tokens?: number | null;
  /** Per-turn cache-write tokens (last LLM call only). */
  cache_write_tokens?: number | null;
  /** Per-turn prompt tokens (last LLM call only). */
  input_tokens?: number | null;
  /** Session-total cache-read tokens (across all LLM calls in the session). */
  total_cache_read_tokens?: number | null;
  /** Session-total cache-write tokens (across all LLM calls in the session). */
  total_cache_write_tokens?: number | null;
  /** Session-total input tokens (across all LLM calls in the session). */
  total_input_tokens?: number | null;
}

/**
 * Compute the prompt-cache hit ratio in [0, 1], or `null` if it is
 * undefined for the current provider / call state.
 *
 * Caller-supplied preference order:
 *
 *   - For "read" we accept either per-turn (`cache_read_tokens`) or
 *     cumulative (`total_cache_read_tokens`).  UI surfaces generally
 *     want cumulative so the user sees the *lifetime* hit rate of
 *     the session, not a single turn's lucky hit.  Per-turn is used
 *     when cumulative is unavailable (e.g. immediately after a
 *     `compute_context_usage` call before `accumulate_llm_usage`).
 *
 *   - For the denominator we use the matching cumulative or per-turn
 *     input/cache-write numbers — never mixing per-turn read with
 *     cumulative input.  Mixing would over-report the ratio (a big
 *     per-turn read on top of a small cumulative input).
 *
 * Returns `null` when:
 *
 *   - the provider has no cache accounting (`getCacheProtocol`
 *     returns `null`),
 *   - cache-read is unknown or zero (no signal yet),
 *   - the denominator is zero or negative (would be division by
 *     zero / a nonsensical ratio).
 */
export function computeCacheHitRate(
  providerId: string | null | undefined,
  usage: CacheUsageLike | null | undefined,
): number | null {
  if (!usage) return null;
  const protocol = getCacheProtocol(providerId);
  if (protocol === null) return null;

  // Prefer cumulative read for the lifetime view; fall back to
  // per-turn when cumulative is not yet known (e.g. fresh session
  // before any LLM call completed).
  const cumulativeRead = numOrNull(usage.total_cache_read_tokens);
  const perTurnRead = numOrNull(usage.cache_read_tokens);
  const read = cumulativeRead ?? perTurnRead;
  if (read == null || read <= 0) return null;

  if (protocol === "openai") {
    // OpenAI: no write event — denominator is just the prompt.
    // Per-turn `input_tokens` is what the provider bills as the
    // total prompt; cumulative `total_input_tokens` is the same
    // concept across the session.  Match the source we used for
    // `read` so we never mix per-turn read with cumulative input.
    const prompt =
      (cumulativeRead != null ? numOrNull(usage.total_input_tokens) : null) ??
      numOrNull(usage.input_tokens);
    if (prompt == null || prompt <= 0) return null;
    return clamp01(read / prompt);
  }

  // protocol === "anthropic"
  // Anthropic denominator: input + read + write, using the same
  // cumulative-vs-per-turn source as `read`.
  const cumulativeWrite = numOrNull(usage.total_cache_write_tokens);
  const perTurnWrite = numOrNull(usage.cache_write_tokens);
  const write =
    cumulativeRead != null
      ? (cumulativeWrite ?? 0)
      : (perTurnWrite ?? 0);

  const input =
    cumulativeRead != null
      ? (numOrNull(usage.total_input_tokens) ?? 0)
      : (numOrNull(usage.input_tokens) ?? 0);

  const denom = input + read + write;
  if (denom <= 0) return null;
  return clamp01(read / denom);
}

/**
 * Format a cache-hit ratio as a percentage string suitable for
 * status-bar / popover rendering.
 *
 *   - `null`  → `null` (let the caller decide whether to render
 *                nothing or a dash).
 *   - numeric → `"12.3%"` (one decimal place is enough for the
 *                resolution a user actually cares about; two would
 *                feel noisy in a 16px badge).  Out-of-range values
 *                are clamped to [0%, 100%] as a visual safety net for
 *                direct callers that bypass `computeCacheHitRate`.
 */
export function formatCacheHitRate(ratio: number | null): string | null {
  if (ratio == null) return null;
  return `${(clamp01(ratio) * 100).toFixed(1)}%`;
}

function numOrNull(v: number | null | undefined): number | null {
  if (v == null) return null;
  if (!Number.isFinite(v)) return null;
  return v;
}

function clamp01(r: number): number {
  if (r < 0) return 0;
  if (r > 1) return 1;
  return r;
}