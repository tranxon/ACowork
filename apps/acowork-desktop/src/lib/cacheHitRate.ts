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
 *   - **OpenAI Chat Completions** (and every OpenAI-compatible
 *     provider routed through the Runtime's `OpenAIProvider` —
 *     Azure, MiniMax, Volcengine/Doubao, custom gateways, …)
 *     reports only `prompt_tokens_details.cached_tokens`; there is
 *     no write concept (the provider auto-caches without a per-call
 *     write event).  The natural denominator is therefore just
 *     `cache_read / prompt_tokens`.
 *
 * The Runtime's `OpenAIProvider` is used for *all* OpenAI-compatible
 * providers and always reports `cache_write_tokens: 0`, while the
 * Anthropic provider reports both read and write.  So the formula is
 * chosen **data-driven**: the presence of cache-write tokens selects
 * the Anthropic denominator, otherwise the OpenAI denominator.  The
 * explicit provider-id classification (`getCacheProtocol`) is kept
 * only as a tiebreaker for Anthropic-family providers that report a
 * read without a write on some call.
 *
 * Two windows are supported, selected by the `window` argument:
 *
 *   - **per-turn** (default) — the last LLM call's real-time counts
 *     (`cache_read_tokens` / `input_tokens`), matching the per-turn
 *     context-usage line rendered next to it.  Used by the input-box
 *     usage popover.
 *
 *   - **cumulative** — the session-lifetime totals
 *     (`total_cache_read_tokens` / `total_input_tokens`), i.e. the
 *     session's overall cache-hit rate.  Used by the session-status
 *     block and the bottom status bar.
 *
 * The two windows are deliberately kept separate: per-turn is a
 * real-time health signal, cumulative is a lagging indicator
 * dominated by early turns.  See ADR-066 §6.
 *
 * Providers that never surface cache accounting (`ollama`,
 * `lmstudio`, …) simply keep `cache_read_tokens` at 0, so the
 * `read <= 0` guard returns `null` and the UI hides the row — no
 * provider allowlist needed.
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
import { formatPercent } from "./utils";

export type CacheProtocol = "openai" | "anthropic";

/**
 * Which time window a cache-hit computation should use.
 *
 * - `"per-turn"`   — the last LLM call's real-time counts
 *                    (`cache_read_tokens` / `input_tokens`).
 * - `"cumulative"` — the session-lifetime totals
 *                    (`total_cache_read_tokens` / `total_input_tokens`).
 */
export type CacheHitWindow = "per-turn" | "cumulative";

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
 * The cache-hit ratio together with the exact numerator and
 * denominator used to compute it.  UI surfaces render the ratio and
 * the two numbers side by side (e.g. `90% | 90K / 100K 缓存命中率`),
 * so the numbers must always be the actual ratio components — never
 * two independent cache counters that happen to look similar.
 */
export interface CacheHitStats {
  /** Hit ratio in [0, 1], or `null` when undefined. */
  ratio: number | null;
  /** Numerator — cache-hit tokens for the selected window
   *  (`cache_read_tokens` per-turn, `total_cache_read_tokens`
   *  cumulative). */
  numerator: number;
  /** Denominator — input tokens for the selected window
   *  (`input_tokens` per-turn, `total_input_tokens` cumulative;
   *  OpenAI-style, or input + read + write Anthropic-style). */
  denominator: number;
}

/**
 * Compute the cache-hit ratio together with its numerator and
 * denominator for the selected window.  See `computeCacheHitRate` for
 * the ratio semantics; this is the single source of truth for both so
 * the displayed numbers always match the displayed percentage.
 *
 * `window` defaults to `"per-turn"` (the last LLM call's real-time
 * view); pass `"cumulative"` for the session-lifetime view.
 */
export function computeCacheHitStats(
  providerId: string | null | undefined,
  usage: CacheUsageLike | null | undefined,
  window: CacheHitWindow = "per-turn",
): CacheHitStats {
  if (!usage) return { ratio: null, numerator: 0, denominator: 0 };

  // Select the window's counts:
  //   - per-turn: the last LLM call's real-time numbers (popup).
  //   - cumulative: the session-lifetime totals (session-status block
  //     and status bar).
  const cumulative = window === "cumulative";
  const read =
    numOrNull(cumulative ? usage.total_cache_read_tokens : usage.cache_read_tokens) ??
    0;
  const write =
    numOrNull(cumulative ? usage.total_cache_write_tokens : usage.cache_write_tokens) ??
    0;
  const input =
    numOrNull(cumulative ? usage.total_input_tokens : usage.input_tokens) ?? 0;

  // Formula selection:
  //   - Explicitly-known providers use their own family (OpenAI ignores
  //     writes, Anthropic includes them) — trust the id over the data.
  //   - Unknown providers (any OpenAI-compatible id routed through the
  //     Runtime's OpenAIProvider — MiniMax, Volcengine, custom, …) are
  //     inferred from the data: the presence of cache-write tokens
  //     selects the Anthropic denominator, otherwise the OpenAI one.
  const protocol = getCacheProtocol(providerId);
  const anthropicStyle =
    protocol === "anthropic" || (protocol === null && write > 0);

  let ratio: number | null = null;
  let denominator = 0;

  if (anthropicStyle) {
    // Anthropic denominator: input + read + write.
    denominator = input + read + write;
    if (read > 0 && denominator > 0) {
      ratio = clamp01(read / denominator);
    }
  } else {
    // OpenAI-style: denominator is just the prompt.
    denominator = input;
    if (read > 0 && input > 0) {
      ratio = clamp01(read / input);
    }
  }

  return { ratio, numerator: read, denominator };
}

/**
 * Compute the prompt-cache hit ratio in [0, 1], or `null` if it is
 * undefined for the current provider / call state.
 *
 * `window` selects the time window (default `"per-turn"`):
 *
 *   - **per-turn** — the last LLM call's real-time counts:
 *     numerator = `cache_read_tokens`, denominator = `input_tokens`
 *     (OpenAI-style) or `input + read + write` (Anthropic-style).
 *     Matches the per-turn context-usage line rendered next to it.
 *
 *   - **cumulative** — the session-lifetime totals:
 *     numerator = `total_cache_read_tokens`, denominator =
 *     `total_input_tokens` (OpenAI-style) or
 *     `total_input + total_read + total_write` (Anthropic-style).
 *     Used by the session-status block and the bottom status bar.
 *
 * Formula selection is **data-driven** (see the module doc): the
 * presence of cache-write tokens selects the Anthropic denominator
 * (`input + read + write`); otherwise the OpenAI denominator
 * (`prompt`).  `getCacheProtocol` is consulted only as a tiebreaker
 * for Anthropic-family providers that report a read without a write
 * on some call.
 *
 * Returns `null` when:
 *
 *   - cache-read is unknown or zero (no signal yet — includes
 *     providers like `ollama` that never report cache tokens),
 *   - the denominator is zero or negative (would be division by
 *     zero / a nonsensical ratio).
 */
export function computeCacheHitRate(
  providerId: string | null | undefined,
  usage: CacheUsageLike | null | undefined,
  window: CacheHitWindow = "per-turn",
): number | null {
  return computeCacheHitStats(providerId, usage, window).ratio;
}

/**
 * Whether the given usage carries any cache accounting for the
 * selected window — the UI gate for rendering the cache row.
 *
 * `window` defaults to `"per-turn"`:
 *
 *   - **per-turn** — true when the current round has cache activity,
 *     OR the session has seen cache activity (so a 0-hit round is
 *     rendered as `— | 0 / N` rather than hidden).
 *   - **cumulative** — true when the session-lifetime totals are
 *     non-zero.
 */
export function hasCacheData(
  usage: CacheUsageLike | null | undefined,
  window: CacheHitWindow = "per-turn",
): boolean {
  if (!usage) return false;
  if (window === "cumulative") {
    return (
      (usage.total_cache_read_tokens ?? 0) > 0 ||
      (usage.total_cache_write_tokens ?? 0) > 0
    );
  }
  return (
    (usage.cache_read_tokens ?? 0) > 0 ||
    (usage.cache_write_tokens ?? 0) > 0 ||
    (usage.total_cache_read_tokens ?? 0) > 0 ||
    (usage.total_cache_write_tokens ?? 0) > 0
  );
}

/**
 * Format a cache-hit ratio as a percentage string suitable for
 * status-bar / popover rendering.
 *
 *   - `null`  → `null` (let the caller decide whether to render
 *                nothing or a dash).
 *   - numeric → `"12%"` (integer, no decimals — deliberately matched
 *                to the context-usage percentage via the shared
 *                `formatPercent` helper so the two stay visually
 *                aligned when shown side by side).  Out-of-range
 *                values are clamped to [0%, 100%] as a visual safety
 *                net for direct callers that bypass
 *                `computeCacheHitRate`.
 */
export function formatCacheHitRate(ratio: number | null): string | null {
  if (ratio == null) return null;
  return `${formatPercent(clamp01(ratio) * 100)}%`;
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